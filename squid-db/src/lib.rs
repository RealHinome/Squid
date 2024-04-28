#![forbid(unsafe_code)]
#![deny(dead_code, unused_imports, unused_mut, missing_docs)]
//! # squid-db
//!
//! internal database used by Squid to store tokenized texts.

/// Compresses bytes to reduce size.
#[cfg(feature = "compress")]
mod compress;
mod ttl;

use serde::Serialize;
use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    fs::{create_dir, read_dir, File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    marker::PhantomData,
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::sync::RwLock as AsyncRwLock;
#[cfg(feature = "logging")]
use tracing::trace;
use ttl::TTL;

const SOURCE_DIRECTORY: &str = "./data/";
const FILE_EXT: &str = "bin";
const MAX_ENTRIES_PER_FILE: u16 = 10_000;

/// Database errors.
#[derive(Debug)]
pub enum DbError {
    /// Main directory haven't been created.
    DirCreationFailed,
    /// An error with absolutely no details.
    Unspecified,
    /// The compression failed.
    #[cfg(feature = "compress")]
    FailedCompression,
    /// The deserialization failed.
    FailedDeserialization,
    /// The serialization failed.
    FailedSerialization,
    /// Error while reading data.
    FailedReading,
    /// Failed unwrap Rwlock or Mutex for writing.
    FailedWriting,
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            DbError::DirCreationFailed => write!(f, "The directory could not be created."),
            DbError::Unspecified => write!(f, "Unknown error"),
            #[cfg(feature = "compress")]
            DbError::FailedCompression => write!(f, "An error occurred during compression"),
            DbError::FailedDeserialization => write!(f, "An error occurred during deserialization"),
            DbError::FailedSerialization => write!(f, "An error occurred during serialization, check the serde implementation"),
            DbError::FailedReading => write!(f, "The data was not read correctly"),
            DbError::FailedWriting => write!(f, "Cannot get Rwlock write"),
        }
    }
}

impl Error for DbError {}

/// Attributes required for TTL management.
pub trait Attributes {
    /// Unique identifier for the sentence.
    fn id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }
    /// Duration, in seconds, of sentence retention.
    fn ttl(&self) -> Option<u64> {
        None
    }
}

/// Structure representing the database world.
#[derive(Serialize, PartialEq, Debug)]
pub struct World<T>(pub Vec<T>)
where
    T: serde::Serialize
        + serde::de::DeserializeOwned
        + std::marker::Send
        + std::marker::Sync
        + 'static;

/// Structure representing one instance of the database.
#[derive(Debug)]
#[allow(dead_code)]
pub struct Instance<
    T: serde::Serialize
        + serde::de::DeserializeOwned
        + std::marker::Send
        + std::marker::Sync
        + 'static
        + Attributes,
> {
    /// File writing new entries.
    /// There is no need to re-open the file each time.
    file: File,
    /// Index to link an ID to a file.
    /// This allows the file to be targeted for modification or deletion.
    index: BTreeMap<String, String>,
    /// TTL manager.
    ttl: Option<Arc<RwLock<TTL<T>>>>,
    /// Data saved on disk.
    pub entries: Vec<T>,
    /// Caching of data to be written to avoid overload and bottlenecks.
    memtable: Vec<T>,
    /// After how many kb the data is written hard to the disk.
    /// Set to 0 to deactivate the memory table.
    memtable_flush_size_in_kb: usize,
    phantom: PhantomData<T>,
}

impl<T> Instance<T>
where
    T: serde::Serialize
        + serde::de::DeserializeOwned
        + Attributes
        + std::marker::Send
        + std::marker::Sync
        + 'static,
{
    /// Create a new database instance.
    ///
    /// # Examples
    /// ```rust
    /// use serde::{Deserialize, Serialize};
    /// use squid_db::{Instance, Attributes};
    ///
    /// #[derive(Serialize, Deserialize)]
    /// struct Entity {
    ///     data: String,
    /// }
    ///
    /// impl Attributes for Entity {}
    ///
    /// let instance: Instance<Entity> = Instance::new(0).unwrap();
    /// //... then you can do enything with the instance.
    /// ```
    pub fn new(memtable_flush_size_in_kb: usize) -> Result<Self, DbError> {
        let (entires, index, file) = load::<T>()?;

        Ok(Self {
            file: file.unwrap_or_else(|| {
                let path = PathBuf::from(SOURCE_DIRECTORY).join(format!(
                    "{}.{}",
                    uuid::Uuid::new_v4(),
                    FILE_EXT
                ));

                OpenOptions::new()
                    .read(true)
                    .append(true)
                    .create(true)
                    .open(&path)
                    .unwrap_or_else(|_| {
                        panic!(
                            "failed to create new file on {}",
                            path.to_string_lossy()
                        )
                    })
            }),
            index,
            ttl: None,
            entries: entires.0,
            memtable: Vec::new(),
            memtable_flush_size_in_kb,
            phantom: PhantomData,
        })
    }

    /// Start TTL manager.
    /// This can results in higher memory consumption.
    ///
    /// # Examples
    /// ```no_run,rust
    /// use serde::{Deserialize, Serialize};
    /// use squid_db::{Instance, Attributes};
    ///
    /// #[derive(Serialize, Deserialize)]
    /// struct Entity {
    ///     id: String,
    ///     data: String,
    ///     love: bool,
    ///     lifetime: u64,
    /// }
    ///
    /// impl Attributes for Entity {
    ///     fn id(&self) -> String {
    ///         self.id.clone()
    ///     }
    ///
    ///     fn ttl(&self) -> Option<u64> {
    ///         Some(self.lifetime)
    ///     }
    /// }
    ///
    /// let mut instance: Instance<Entity> = Instance::new(0).unwrap();
    ///
    /// instance.set(Entity {
    ///     id: "U1".to_string(),
    ///     data: "I do not know if my french teaher like me...".to_string(),
    ///     love: false,
    ///     lifetime: 0, // permanent sentence.
    /// });
    ///
    /// instance.set(Entity {
    ///     id: "U2".to_string(),
    ///     data: "It starts with A! My love?".to_string(),
    ///     love: true,
    ///     lifetime: 500, // because love only lasts 500 seconds.
    /// });
    ///
    /// instance.start_ttl();
    /// ```
    pub fn start_ttl(self) -> Arc<AsyncRwLock<Instance<T>>> {
        let this = Arc::new(AsyncRwLock::new(self));
        let ttl_manager =
            Arc::new(RwLock::new(ttl::TTL::new(Arc::clone(&this))));

        let (ttl, instance) = (Arc::clone(&ttl_manager), Arc::clone(&this));
        tokio::task::spawn(async move {
            for entry in &instance.read().await.entries {
                if let Some(expire) = entry.ttl() {
                    let _ = ttl.write().unwrap().add_entry(entry.id(), expire);
                }
            }
        });

        ttl_manager.write().unwrap().init();
        /*if let Ok(mut writer) = this.write() {
            writer.ttl = Some(Arc::new(RwLock::new(ttl_manager)));
        }*/

        this
    }

    /// Add a new entry to the database.
    ///
    /// # Examples
    /// ```rust
    /// use serde::{Deserialize, Serialize};
    /// use squid_db::{Instance, Attributes};
    ///
    /// #[derive(Serialize, Deserialize)]
    /// struct Entity {
    ///     data: String,
    ///     love_him: bool,
    /// }
    ///
    /// impl Attributes for Entity {}
    ///
    /// let mut instance: Instance<Entity> = Instance::new(0).unwrap();
    ///
    /// instance.set(Entity {
    ///     data: "I really like my classmate, Julien".to_string(),
    ///     love_him: false,
    /// });
    ///
    /// instance.set(Entity {
    ///     data: "But I do not speak to Julien".to_string(),
    ///     love_him: true,
    /// });
    /// ```
    pub fn set(&mut self, data: T) -> Result<(), DbError> {
        if let Some(timestamp) = data.ttl() {
            self.ttl
                .as_ref()
                .and_then(|ttl| ttl.write().ok())
                .map(|mut ttl| ttl.add_entry(data.id(), timestamp))
                .transpose()?;
        }

        #[cfg(feature = "logging")]
        trace!(id = data.id(), "Added new entry with ID {}.", data.id());

        match self.memtable_flush_size_in_kb {
            0 => {
                #[cfg(not(feature = "compress"))]
                let encoded = bincode::serialize(&data)
                    .map_err(|_| DbError::FailedSerialization)?;

                self.save(&encoded)?
            },
            max_kb_size => {
                self.memtable.push(data);

                if max_kb_size
                    < (self.memtable.len() * std::mem::size_of::<T>()) / 1000
                {
                    self.flush().map_err(|_| DbError::Unspecified)?
                }
            },
        }

        Ok(())
    }

    /// Deletes a record from the data based on its unique identifier.
    pub fn delete(&self, id: String) -> Result<(), DbError> {
        if let Some(file_name) = self.index.get(&id) {
            let file =
                File::open(PathBuf::from(SOURCE_DIRECTORY).join(file_name))
                    .map_err(|_| DbError::FailedReading)?;
            let reader = BufReader::new(file);

            let lines: Vec<Vec<u8>> = reader
                .lines()
                .map_while(Result::ok)
                .map(|entry| entry.as_bytes().to_vec())
                .collect();

            let index_to_delete = lines.iter().position(|line| {
                if let Ok(data) = bincode::deserialize::<T>(line) {
                    return data.id() == id;
                }
                false
            });

            if let Some(index) = index_to_delete {
                let mut file = OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(PathBuf::from(SOURCE_DIRECTORY).join(file_name))
                    .map_err(|_| DbError::Unspecified)?;

                lines.iter().enumerate().for_each(|(i, line)| {
                    if i != index {
                        writeln!(file, "{}", String::from_utf8_lossy(line))
                            .unwrap_or_default();
                    }
                });

                #[cfg(feature = "logging")]
                trace!(
                    id = id,
                    file = file_name,
                    "Entry {} deleted from {}",
                    id,
                    file_name
                );
            }
        } else {
            // TODO: support memtable deletation.
            //self.memtable.retain(|entry| entry.id() != id);
            return Err(DbError::Unspecified);
        }

        Ok(())
    }

    /// Append one data to the file.
    #[inline(always)]
    #[allow(unused)]
    fn save(&mut self, buf: &[u8]) -> Result<(), DbError> {
        let reader = io::BufReader::new(&self.file);
        let mut line_count = 0;
        for _line in reader.lines() {
            line_count += 1;
        }

        let mut buffer: Vec<u8> = vec![];

        buffer.extend_from_slice(buf);
        buffer.extend_from_slice(b"\n");

        self.file
            .write_all(&buffer)
            .map_err(|_| DbError::Unspecified)?;

        if line_count + 1 >= MAX_ENTRIES_PER_FILE.into() {
            let path = PathBuf::from(SOURCE_DIRECTORY).join(format!(
                "{}.{}",
                uuid::Uuid::new_v4(),
                FILE_EXT
            ));

            self.file = OpenOptions::new()
                .read(true)
                .append(true)
                .create(true)
                .open(&path)
                .unwrap_or_else(|_| {
                    panic!(
                        "failed to create new file on {}",
                        path.to_string_lossy()
                    )
                });
        }

        Ok(())
    }

    /// Saves the data contained in the buffer to the hard disk.
    pub fn flush(&mut self) -> Result<(), DbError> {
        let reader = io::BufReader::new(&self.file);
        let mut line_count = 0;
        for _line in reader.lines() {
            line_count += 1;
        }

        if line_count + self.memtable.len() > MAX_ENTRIES_PER_FILE.into() {
            // If we just write all, number of lines will exceed maximum allowed.
            // So, we will split into two different files.
            let mut buffer: Vec<u8> = Vec::with_capacity(self.memtable.len());

            let mut file_limit = (MAX_ENTRIES_PER_FILE as usize) - line_count;
            for n in 0..file_limit {
                buffer.extend_from_slice(
                    &bincode::serialize(&self.memtable[n])
                        .map_err(|_| DbError::FailedSerialization)?,
                );
                buffer.extend_from_slice(b"\n");
            }

            self.file
                .write_all(&buffer)
                .map_err(|_| DbError::Unspecified)?;
            self.file.flush().map_err(|_| DbError::Unspecified)?;

            let path = PathBuf::from(SOURCE_DIRECTORY).join(format!(
                "{}.{}",
                uuid::Uuid::new_v4(),
                FILE_EXT
            ));

            self.file = OpenOptions::new()
                .read(true)
                .append(true)
                .create(true)
                .open(&path)
                .unwrap_or_else(|_| {
                    panic!(
                        "failed to create new file on {}",
                        path.to_string_lossy()
                    )
                });

            for _ in 1..(line_count + self.memtable.len()
                - (MAX_ENTRIES_PER_FILE as usize))
            {
                file_limit += 1;

                buffer.extend_from_slice(
                    &bincode::serialize(&self.memtable[file_limit])
                        .map_err(|_| DbError::FailedSerialization)?,
                );
                buffer.extend_from_slice(b"\n");
            }

            self.file
                .write_all(&buffer)
                .map_err(|_| DbError::Unspecified)?;
        } else {
            let mut buffer: Vec<u8> = Vec::with_capacity(self.memtable.len());

            for data in &self.memtable {
                buffer.extend_from_slice(
                    &bincode::serialize(&data)
                        .map_err(|_| DbError::FailedSerialization)?,
                );
                buffer.extend_from_slice(b"\n");
            }

            self.file
                .write_all(&buffer)
                .map_err(|_| DbError::Unspecified)?;

            self.memtable.clear();
        }

        Ok(())
    }
}

/// Loads data from the file.
#[inline(always)]
fn load<T>(
) -> Result<(World<T>, BTreeMap<String, String>, Option<File>), DbError>
where
    T: serde::de::DeserializeOwned
        + serde::Serialize
        + Attributes
        + std::marker::Send
        + std::marker::Sync
        + 'static,
{
    let mut world: World<T> = World(Vec::new());
    let mut index: BTreeMap<String, String> = BTreeMap::new();
    let mut uncomplete_file: Option<File> = None;

    let _ = create_dir(SOURCE_DIRECTORY);

    for entry in read_dir(SOURCE_DIRECTORY)
        .map_err(|_| DbError::FailedReading)?
        .collect::<Result<Vec<_>, io::Error>>()
        .map_err(|_| DbError::FailedReading)?
    {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(entry.path())
            .map_err(|_| DbError::Unspecified)?;

        let reader = BufReader::new(&file);
        let mut file_lines: u16 = 0;

        for line in reader.lines() {
            file_lines += 1;

            let line_data: T = bincode::deserialize(
                line.map_err(|_| DbError::FailedReading)?.as_bytes(),
            )
            .map_err(|_| DbError::FailedDeserialization)?;

            index.insert(
                line_data.id(),
                entry.file_name().into_string().unwrap_or_default(),
            );
            world.0.push(line_data);
        }

        if file_lines < MAX_ENTRIES_PER_FILE {
            uncomplete_file = Some(file);
        }
    }

    Ok((world, index, uncomplete_file))
}
