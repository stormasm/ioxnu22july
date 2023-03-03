# Underground Guide to testing IOx locally

This document explains how to run IOx locally (for locally
profiling, for example) similarly to how it is deployed in production
but from source in your local development environment where you can
run low key experiments.

This is an "underground" guide in the sense that it is not meant to
define an official setup for profiling or benchmarking and should not
be used for such. It is provided in the hope it will be helpful for
developers.

## Step 1: Build IOx

Build IOx for release with pprof:

```shell
cargo build --release --features=pprof
```

## Step 2: Start redpanda and postgres

Now, start up redpanda and postgres locally in docker containers:
```shell
# get rskafka from https://github.com/influxdata/rskafka
cd rskafka
# Run redpanda on localhost:9010
docker-compose -f docker-compose-redpanda.yml up &
# now run postgres
docker run -p 5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust postgres &
```

Of course, you can also use locally running services (if, for example,
you have postgres running locally on port 5432).


# Step 3: Do one time initialization setup


```shell
# initialize the catalog
INFLUXDB_IOX_WRITE_BUFFER_TYPE=kafka \
INFLUXDB_IOX_WRITE_BUFFER_ADDR=localhost:9010 \
INFLUXDB_IOX_WRITE_BUFFER_AUTO_CREATE_TOPICS=10 \
INFLUXDB_IOX_CATALOG_DSN=postgres://postgres@localhost:5432/postgres \
OBJECT_STORE=file \
DATABASE_DIRECTORY=~/data_dir \
LOG_FILTER=debug \
./target/release/influxdb_iox catalog setup

# initialize the kafka topic
INFLUXDB_IOX_WRITE_BUFFER_TYPE=kafka \
INFLUXDB_IOX_WRITE_BUFFER_ADDR=localhost:9010 \
INFLUXDB_IOX_WRITE_BUFFER_AUTO_CREATE_TOPICS=10 \
INFLUXDB_IOX_CATALOG_DSN=postgres://postgres@localhost:5432/postgres \
OBJECT_STORE=file \
DATABASE_DIRECTORY=~/data_dir \
LOG_FILTER=debug \
./target/release/influxdb_iox catalog topic update iox-shared
```

## Inspecting Catalog and Kafka / Redpanda state

Depending on what you are trying to do, you may want to inspect the
catalog and/or the contents of Kafka / Redpands.

You can run psql like this to inspect the catalog:
```shell
psql -h localhost -p 5432 -U postgres
```

```sql
postgres=# set search_path = iox_catalog;
SET
postgres=# \d
                         List of relations
   Schema    |             Name              |   Type   |  Owner
-------------+-------------------------------+----------+----------
 iox_catalog | _sqlx_migrations              | table    | postgres
 iox_catalog | column_name                   | table    | postgres
 iox_catalog | column_name_id_seq            | sequence | postgres
 iox_catalog | kafka_topic                   | table    | postgres
 iox_catalog | kafka_topic_id_seq            | sequence | postgres
 iox_catalog | namespace                     | table    | postgres
 iox_catalog | namespace_id_seq              | sequence | postgres
 iox_catalog | parquet_file                  | table    | postgres
 iox_catalog | parquet_file_id_seq           | sequence | postgres
 iox_catalog | partition                     | table    | postgres
 iox_catalog | partition_id_seq              | sequence | postgres
 iox_catalog | processed_tombstone           | table    | postgres
 iox_catalog | query_pool                    | table    | postgres
 iox_catalog | query_pool_id_seq             | sequence | postgres
 iox_catalog | sequencer                     | table    | postgres
 iox_catalog | sequencer_id_seq              | sequence | postgres
 iox_catalog | sharding_rule_override        | table    | postgres
 iox_catalog | sharding_rule_override_id_seq | sequence | postgres
 iox_catalog | table_name                    | table    | postgres
 iox_catalog | table_name_id_seq             | sequence | postgres
 iox_catalog | tombstone                     | table    | postgres
 iox_catalog | tombstone_id_seq              | sequence | postgres
(22 rows)

postgres=#
```

You can mess with redpanda using `docker exec redpanda-0 rpk` like this:

```shell
$ docker exec redpanda-0 rpk topic list
NAME        PARTITIONS  REPLICAS
iox-shared  1           1
```


# Step 4: Run the services

## Run Router on port 8080/8081 (http/grpc)
```shell
INFLUXDB_IOX_BIND_ADDR=localhost:8080 \
INFLUXDB_IOX_GRPC_BIND_ADDR=localhost:8081 \
INFLUXDB_IOX_WRITE_BUFFER_TYPE=kafka \
INFLUXDB_IOX_WRITE_BUFFER_ADDR=localhost:9010 \
INFLUXDB_IOX_WRITE_BUFFER_AUTO_CREATE_TOPICS=10 \
INFLUXDB_IOX_CATALOG_DSN=postgres://postgres@localhost:5432/postgres \
OBJECT_STORE=file \
DATABASE_DIRECTORY=~/data_dir \
LOG_FILTER=info \
./target/release/influxdb_iox run router
```


## Run Ingester on port 8083/8083 (http/grpc)
```shell
INFLUXDB_IOX_BIND_ADDR=localhost:8083 \
INFLUXDB_IOX_GRPC_BIND_ADDR=localhost:8084 \
INFLUXDB_IOX_WRITE_BUFFER_TYPE=kafka \
INFLUXDB_IOX_WRITE_BUFFER_ADDR=localhost:9010 \
xINFLUXDB_IOX_WRITE_BUFFER_AUTO_CREATE_TOPICS=10 \
INFLUXDB_IOX_WRITE_BUFFER_PARTITION_RANGE_START=0 \
INFLUXDB_IOX_WRITE_BUFFER_PARTITION_RANGE_END=0 \
INFLUXDB_IOX_PAUSE_INGEST_SIZE_BYTES=5000000000 \
INFLUXDB_IOX_PERSIST_MEMORY_THRESHOLD_BYTES=4000000000 \
INFLUXDB_IOX_CATALOG_DSN=postgres://postgres@localhost:5432/postgres \
INFLUXDB_IOX_MAX_HTTP_REQUEST_SIZE=100000000 \
OBJECT_STORE=file \
DATABASE_DIRECTORY=~/data_dir \
LOG_FILTER=info \
./target/release/influxdb_iox run ingester
```


# Step 5: Ingest data

Now you can post data to `http://localhost:8080` with your favorite load generating tool

My favorite is https://github.com/alamb/low_card

To run:
```shell
git clone git@github.com:alamb/low_card.git
cd low_card
cargo run --release
```

Then tweak the parameters in `main.rs` code to change the shape of the
data. The default settings at the time of this writing would result in
posting fairly large requests (necessitating the
`INFLUXDB_IOX_MAX_HTTP_REQUEST_SIZE` setting above)


# Step 6: Profile

See [`profiling.md`](./profiling.md).
