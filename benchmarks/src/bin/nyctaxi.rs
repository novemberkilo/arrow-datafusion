// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Apache Arrow Rust Benchmarks

use std::collections::HashMap;
use std::path::PathBuf;
use std::process;
use std::time::Instant;

use datafusion::arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use datafusion::arrow::util::pretty;

use datafusion::error::Result;
use datafusion::execution::context::{ExecutionConfig, ExecutionContext};

use datafusion::physical_plan::collect;
use datafusion::physical_plan::csv::CsvReadOptions;
use structopt::StructOpt;

#[cfg(feature = "snmalloc")]
#[global_allocator]
static ALLOC: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

#[derive(Debug, StructOpt)]
#[structopt(name = "Benchmarks", about = "Apache Arrow Rust Benchmarks.")]
struct Opt {
    /// Activate debug mode to see query results
    #[structopt(short, long)]
    debug: bool,

    /// Number of iterations of each test run
    #[structopt(short = "i", long = "iterations", default_value = "3")]
    iterations: usize,

    /// Number of partitions to process in parallel
    #[structopt(short = "p", long = "partitions", default_value = "2")]
    partitions: usize,

    /// Batch size when reading CSV or Parquet files
    #[structopt(short = "s", long = "batch-size", default_value = "8192")]
    batch_size: usize,

    /// Path to data files
    #[structopt(parse(from_os_str), required = true, short = "p", long = "path")]
    path: PathBuf,

    /// File format: `csv` or `parquet`
    #[structopt(short = "f", long = "format", default_value = "csv")]
    file_format: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opt = Opt::from_args();
    println!("Running benchmarks with the following options: {:?}", opt);

    let config = ExecutionConfig::new()
        .with_target_partitions(opt.partitions)
        .with_batch_size(opt.batch_size);
    let mut ctx = ExecutionContext::with_config(config);

    let path = opt.path.to_str().unwrap();

    match opt.file_format.as_str() {
        "csv" => {
            let schema = nyctaxi_schema();
            let options = CsvReadOptions::new().schema(&schema).has_header(true);
            ctx.register_csv("tripdata", path, options)?
        }
        "parquet" => ctx.register_parquet("tripdata", path)?,
        other => {
            println!("Invalid file format '{}'", other);
            process::exit(-1);
        }
    }

    datafusion_sql_benchmarks(&mut ctx, opt.iterations, opt.debug).await
}

async fn datafusion_sql_benchmarks(
    ctx: &mut ExecutionContext,
    iterations: usize,
    debug: bool,
) -> Result<()> {
    let mut queries = HashMap::new();
    queries.insert("fare_amt_by_passenger", "SELECT passenger_count, MIN(pickup_datetime), MIN(fare_amount), MAX(fare_amount), SUM(fare_amount) FROM tripdata GROUP BY passenger_count");
    for (name, sql) in &queries {
        println!("Executing '{}'", name);
        for i in 0..iterations {
            let start = Instant::now();
            execute_sql(ctx, sql, debug).await?;
            println!(
                "Query '{}' iteration {} took {} ms",
                name,
                i,
                start.elapsed().as_millis()
            );
        }
    }
    Ok(())
}

async fn execute_sql(ctx: &mut ExecutionContext, sql: &str, debug: bool) -> Result<()> {
    let plan = ctx.create_logical_plan(sql)?;
    let plan = ctx.optimize(&plan)?;
    if debug {
        println!("Optimized logical plan:\n{:?}", plan);
    }
    let physical_plan = ctx.create_physical_plan(&plan).await?;
    let result = collect(physical_plan).await?;
    if debug {
        pretty::print_batches(&result)?;
    }
    Ok(())
}

fn nyctaxi_schema() -> Schema {
    Schema::new(vec![
        Field::new("VendorID", DataType::Utf8, true),
        Field::new("pickup_datetime", DataType::Timestamp(TimeUnit::Microsecond, None), true),
        Field::new("dropoff_datetime", DataType::Timestamp(TimeUnit::Microsecond, None), true),
        // Field::new("tpep_pickup_datetime", DataType::Utf8, true),
        // Field::new("tpep_dropoff_datetime", DataType::Utf8, true),
        Field::new("passenger_count", DataType::Int32, true),
        Field::new("trip_distance", DataType::Utf8, true),
        Field::new("RatecodeID", DataType::Utf8, true),
        Field::new("store_and_fwd_flag", DataType::Utf8, true),
        Field::new("PULocationID", DataType::Utf8, true),
        Field::new("DOLocationID", DataType::Utf8, true),
        Field::new("payment_type", DataType::Utf8, true),
        Field::new("fare_amount", DataType::Float64, true),
        Field::new("extra", DataType::Float64, true),
        Field::new("mta_tax", DataType::Float64, true),
        Field::new("tip_amount", DataType::Float64, true),
        Field::new("tolls_amount", DataType::Float64, true),
        Field::new("improvement_surcharge", DataType::Float64, true),
        Field::new("total_amount", DataType::Float64, true),
    ])
}
