use std::{collections::HashMap, path::PathBuf};

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use harvestlib::EventExtractWorker;
use move_core_types::language_storage::StructTag;
use statrs::statistics::Statistics;
use sui_sdk::SuiClientBuilder;
use sui_types::TypeTag;

/// A simple event monitor and library to consume events from the Sui blockchain.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Number of checkpoints to process
    #[arg(short, long, default_value_t = 10)]
    count: u64,

    /// Number of checkpoints to process
    #[arg(long, default_value_t = 5)]
    concurrent: u64,

    /// Whether to follow in real time
    #[arg(short, long, default_value_t = false)]
    follow: bool,

    /// Bottom percentage to suppress
    #[arg(short, long, default_value_t = 0.5)]
    suppress: f64,

    /// URL of Sui full nodes
    #[arg(long, default_value = "https://fullnode.mainnet.sui.io:443")]
    full_node_url: String,

    /// URL of Sui checkpoint nodes
    #[arg(long, default_value = "https://checkpoints.mainnet.sui.io")]
    checkpoints_node_url: String,
}

fn tag_to_short_string(tag_: &TypeTag) -> String {
    match tag_ {
        TypeTag::Struct(struct_tag) => type_to_short_string(struct_tag),
        TypeTag::Vector(type_tag) => format!("Vector<{}>", tag_to_short_string(type_tag)),
        _ => tag_.to_canonical_string(false),
    }
}

fn type_to_short_string(type_: &StructTag) -> String {
    let base = format!("{}::{}", type_.module, type_.name,);

    if type_.type_params.is_empty() {
        base
    } else {
        let type_params = type_
            .type_params
            .iter()
            .map(tag_to_short_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}<{}>", base, type_params)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args = Args::parse();

    let sui_mainnet = SuiClientBuilder::default()
        .build(args.full_node_url)
        .await?;
    println!("Sui mainnet version: {}", sui_mainnet.api_version());

    // Get and print the latest checkpoint
    let latest_checkpoint = sui_mainnet
        .read_api()
        .get_latest_checkpoint_sequence_number()
        .await?;

    let limit = args.count;

    let initial = if args.follow {
        println!(
            "Following the latest checkpoint ({}) ...",
            latest_checkpoint
        );
        latest_checkpoint
    } else {
        println!(
            "Get events from checkpoints {} ... {}",
            (latest_checkpoint - limit).max(0),
            latest_checkpoint
        );
        (latest_checkpoint - limit).max(0)
    };

    // Get a new Custom Worker
    let (executor, mut receiver) = EventExtractWorker::new(
        initial,
        limit,
        |_e| true,
        args.checkpoints_node_url.clone(),
        args.concurrent as usize,
        None,
        Some(PathBuf::from("cache")),
    )
    .await?;

    // spawn a task to process the received data
    let join = tokio::spawn(async move {
        // Histogram of identifiers
        let mut histogram = HashMap::new();
        let mut events_by_package = HashMap::new();

        while let Some((_summary, data)) = receiver.recv().await {
            // Update the histogram
            data.iter().for_each(|(_index, _id, event)| {
                let entry = histogram
                    .entry(event.type_.address)
                    .or_insert((0, HashMap::new()));
                entry.0 += 1;
                let entry = entry.1.entry(event.type_.clone()).or_insert(0);
                *entry += 1;

                let count = events_by_package.entry(event.package_id).or_insert(0);
                *count += 1;
            });
        }

        // Print all entries in the histogram, sorted in descending order of value
        let mut histogram: Vec<_> = histogram.into_iter().collect();
        histogram.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));

        // Sum all events
        let total_events: usize = histogram.iter().map(|(_type_, value)| value.0).sum();
        // Define the cutoff to suppress
        let cutoff = (total_events as f64 * args.suppress / 100.0).round() as usize;
        if cutoff > 0 {
            println!("Suppressing packages with fewer than {} events", cutoff);
        }

        for (type_, value) in histogram.into_iter() {
            if value.0 < cutoff {
                continue;
            }

            println!("\x1b[34m{:<5}\x1b[0m {}", value.0, type_.to_string().red());

            let mut inner_histogram: Vec<_> = value.1.into_iter().collect();
            inner_histogram.sort_by(|a, b| b.1.cmp(&a.1));

            for (type_, value) in inner_histogram.into_iter() {
                println!(
                    "       \x1b[34m{:5}\x1b[0m : {}",
                    value,
                    type_to_short_string(&type_).green()
                );
            }
        }

        println!("\nEvents by package:");
        for (package, count) in &events_by_package {
            println!("\x1b[34m{package:<5}\x1b[0m {count}");
        }
        let total_packages = events_by_package.len();
        let average_events_by_package = events_by_package.values().sum::<usize>() / total_packages;
        let stdev_events_by_package = events_by_package
            .values()
            .map(|&x| x as f64)
            .collect::<Vec<_>>()
            .std_dev();
        println!(
            "Summary: {total_packages} packages, 
            with an average of {average_events_by_package} +- {stdev_events_by_package} events each"
        );
    });

    executor.await?;
    join.await?;
    Ok(())
}
