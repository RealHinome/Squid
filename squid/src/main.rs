#![forbid(unsafe_code)]

mod helpers;
mod models;

#[macro_use]
extern crate lazy_static;

use squid::{
    squid_server::{Squid, SquidServer},
    {AddRequest, LeaderboardRequest, Ranking, Void, Word},
};
use squid_tokenizer::tokenize;
use std::{
    ops::Add,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;
use tonic::{transport::Server, Request, Response, Status};
use tracing::{error, info};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub mod squid {
    tonic::include_proto!("squid");
}
struct SuperSquid {
    algorithm: helpers::database::Algorithm,
    config: models::config::Config,
    instance: Arc<RwLock<squid_db::Instance<models::database::Entity>>>,
}

const FLUSHTABLE_FLUSH_SIZE_KB: usize = 0; // instantly save it.

#[tonic::async_trait]
impl Squid for SuperSquid {
    async fn leaderboard(
        &self,
        request: Request<LeaderboardRequest>,
    ) -> Result<Response<Ranking>, Status> {
        Ok(Response::new(Ranking {
            word: helpers::database::rank(
                self.algorithm.clone(),
                request.into_inner().length as usize,
            )
            .iter()
            .map(|(word, occurence)| Word {
                word: word.to_string(),
                occurence: (*occurence).try_into().unwrap_or_default(),
            })
            .collect::<Vec<_>>(),
        }))
    }

    async fn add(
        &self,
        request: Request<AddRequest>,
    ) -> Result<Response<Void>, Status> {
        let data = request.into_inner();

        helpers::database::set(
            &self.config,
            Arc::clone(&self.instance),
            self.algorithm.clone(),
            models::database::Entity {
                id: uuid::Uuid::new_v4().to_string(),
                original_text: None,
                post_processing_text: tokenize(&data.sentence).map_err(
                    |error| {
                        error!(
                            "Failed to tokenize {:?}: {}",
                            data.sentence, error
                        );
                        Status::invalid_argument("failed to tokenize sentence")
                    },
                )?,
                lang: "fr".to_string(),
                meta: if data.lifetime == 0 {
                    String::default()
                } else {
                    format!(
                        "expire_at:{}",
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .add(Duration::from_secs(data.lifetime))
                            .as_secs()
                    )
                },
            },
        )
        .await
        .unwrap();

        Ok(Response::new(Void {}))
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_file(true)
                .with_line_number(true)
                .with_thread_ids(true),
        )
        .init();

    let config = helpers::config::read();

    // Start database.
    let db_instance: squid_db::Instance<models::database::Entity> =
        squid_db::Instance::new(FLUSHTABLE_FLUSH_SIZE_KB).unwrap();
    info!(
        "Loaded instance with {} entities.",
        db_instance.entries.len()
    );

    // Chose algorithm.
    let mut algo = match config.service.algorithm {
        models::config::Algorithm::Hashmap => {
            squid_algorithm::hashtable::MapAlgorithm::default()
        },
    };

    for data in &db_instance.entries {
        for str in data.post_processing_text.split_whitespace() {
            if !config.service.exclude.contains(&str.to_string()) {
                match config.service.message_type {
                    models::config::MessageType::Hashtag => {
                        if str.starts_with('#') {
                            algo.set(str)
                        }
                    },
                    models::config::MessageType::Word => {
                        if !str.starts_with('#') {
                            algo.set(str)
                        }
                    },
                    _ => algo.set(str),
                }
            }
        }
    }

    // Init TTL.
    let instance = db_instance.start_ttl();

    // Remove entires to reduce ram usage.
    instance.write().await.entries.clear();

    /*let ctrlc_instance = Arc::clone(&instance);
    ctrlc::set_handler(move || {
        let ctrlc_instance = Arc::clone(&ctrlc_instance);
        if FLUSHTABLE_FLUSH_SIZE_KB > 0 {
            info!("Flush memtable...");
            tokio::task::spawn(async move {
                if let Err(err) = ctrlc_instance.write().await.flush() {
                    error!(
                        "Some data haven't been flushed from memtable: {}",
                        err
                    );
                }
            });
        }

        std::process::exit(0);
    })
    .expect("Failed to set Ctrl+C handler");*/

    let addr = format!("0.0.0.0:{}", config.port.unwrap_or(50051))
        .parse()
        .unwrap();

    info!("Server started on {}", addr);

    Server::builder()
        .add_service(SquidServer::new(SuperSquid {
            algorithm: helpers::database::Algorithm::Map(algo),
            config,
            instance,
        }))
        .serve(addr)
        .await
        .unwrap();
}
