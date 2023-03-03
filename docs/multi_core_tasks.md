# Patterns for CPU usage in the Runtime

As discussed on https://github.com/influxdata/influxdb_iox/pull/221 and https://github.com/influxdata/influxdb_iox/pull/226, we needed a strategy for handling I/O as well as CPU heavy requests in a flexible manner that will allow making appropriate tradeoffs between resource utilization and availability under load.

## TLDR;

1. Use only async I/O via `tokio` for socket communication. It is ok to use either blocking (e.g. `std::fs::File`) or async APIs (e.g. `tokio::fs::File`) for local File I/O.

2. All CPU bound tasks should be scheduled on the separate application level `thread_pool` (which can be another tokio executor but should be separate from the executor that handles I/O).

We will work, over time, to migrate the rest of the codebase to use these patterns.

## Background

At a high level there are two different kinds of work the server does:

1. Responding to external I/O requests (a.k.a. responding to `GET /ping`, `POST /api/v2/write`, or the various gRPC APIs), or making internal I/O requests such as to read or write to a local file.

2. CPU heavy tasks such data parsing, format conversion, or query processing

## Desired Properties / Rationale

We wish to have the following properties in the runtime:

1. Incoming requests are always processed and responded to in a "timely" manner, even while the server is load / overloaded. For example, even if the server is currently consuming 100% of the CPU doing useful work (converting data, running queries, etc.), it still must be able to answer a `/ping` request to tell observers that it is still alive and not be taken out of a load balancer rotation or killed by Kubernetes.

2. Ability to (eventually) control the priority of CPU heavy tasks, so we can prioritize certain tasks over others (e.g. query requests over background data rearrangement). This document focuses on the split between I/O and CPU tasks, not the priority scheduling for CPU heavy tasks, which is deferred to a future document.


## Guidelines

The above desires, leads us to the following guidelines:

### Do not use blocking I/O for network.

**What**: All socket I/O should be done with `async` functions in tokio. It is ok to use either blocking (e.g. `std::fs::File`) or async APIs (e.g. `tokio::fs::File`) for local File I/O.

**Rationale**: Using async functions in tokio ensures that we do not tie up threads which are servicing I/O requests while blocking on the network. At the time of this writing, our assumption is that on systems where IOx will be deployed, Filesystem I/O bandwidth and IOPS will be sufficient to avoid significant blocking, even under high load.

It is ok to use either blocking (e.g. `std::fs::File`) or  async APIs for local File I/O.

This can not always be done (e.g. with a library such as parquet writer which is not `async`). In such cases, using `tokio::task::spawn_blocking` should be used to perform the file I/O.

### All CPU heavy work should be done on the single app level worker pool, separate from the tokio runtime handling IO

**What**: All CPU heavy work should be done on the app level worker pool. We provide a `thread_pool` interface that interacts nicely with async tasks (e.g. that allows an async task to `await` for a CPU heavy task to complete).

**Rationale**: A single app level worker pool gives us a single place to control work priority, eventually, so that tasks such as compaction of large data files can have lower precedence than incoming queries. By using a different pool than the main tokio runtime, with a limited number of threads, we avoid over-saturating the CPU with OS threads and thereby starving the limited number tokio I/O threads. A separate, single app level pool also limits the number of underlying OS CPU threads which are spawned, even under heavy load, keeping thread context switching overhead low.

There will, of course, always be a judgment call to be made of where "CPU bound work" starts and "work acceptable for I/O processing"  ends. A reasonable rule of thumb is if a job will *always* be completed in less than 100ms then that is probably fine for an I/O thread). This number may be revised as we tune the system.

The following is a specific example of why it is important to be cautious when receiving RPC work and ensuring only minimal CPU work is done before handing off to a pool of workers. In some versions of InfluxDB, the `/write` HTTP handlers read, parse, allocate and forward the points to the InfluxDB TSM engine for writing to the Write Buffer. The Write Buffer is the first place where back pressure appears. The problem with this approach is you can have 1,000s of HTTP requests in flight parsing, allocating, consuming huge amounts of memory and CPU resources, causing servers to fall over. This situation can be avoided by forwarding the work to a work queue (from parsing onwards).


## Examples

### Worked Examples

In this section, we show how this design would be applied to a hypothetical 4 core system. For the purposes of illustration, we ignore CPU needs of other processes and the Operating System itself. We examine the following two configurations that an operator might desire and their effect on overall CPU utilization and ping response latency.

*Minimal Response Latency*: To achieve minimal response latency, the operator could configure the system with 3 threads in the CPU threadpool and 1 thread in the tokio I/O task pool. This division will ensure there is always at least one core available to respond to ping requests.

*Maximum CPU Utilization*: To achieve maximum CPU utilization (all cores are busy if there is work to do), the operator could configure the system with 4 threads in the CPU threadpool and 1 thread in the tokio I/O task pool. Given 5 threads and 4 cores, while there will always be a thread available to handle I/O requests, it will be multiplexed by the operating system with the threads doing CPU heavy work.

**Note**: In this document, we assume a simple implementation where there is a single threadpool used for both high and low priority CPU heavy work. While more sophisticated designs are possible (e.g.  segregated threadpools, task spilling, etc.), this document focuses on the split between I/O and CPU, not priority for CPU heavy tasks which is tracked, along with other resource management needs, [here](https://github.com/influxdata/influxdb_iox/issues/241)

#### Heavy Query Processing

In this scenario, 10 concurrent request for CPU heavy queries come in. The I/O thread handles the query requests and passes the processing to the CPU threads.

*Minimal Response Latency*: 3 CPU heavy threads process the queries and the I/O thread remains ready for new requests. The system will appear 75% utilized (using 3 of 4 cores). The remaining core is available to handle ping requests with minimal additional latency.

*Maximum CPU Utilization*: In this case, 4 CPU heavy threads process the queries and the I/O thread still remains available for new requests. The system cores will be 100% utilized. Incoming requests will be handled by the I/O thread, but that thread will only have ~80% of a CPU core available and thus ping responses may be delayed.


#### Query Processing with Low Priority Background Work

In this scenario, 10 concurrent requests for CPU heavy queries come in while there are 2 background jobs (e.g. storage reorganization) running at "low" priority. The I/O thread handles the query requests and passes the processing to the CPU threads as before.

With a little hand waving, in either configuration, the CPU heavy threads eventually pause their background work and begin processing the higher priority query loads. The details of how priority scheduling of CPU tasks is handled is deferred to another document.

In both the *Minimal Response Latency* and *Maximum CPU Utilization* configurations there are respectively 1 or 2 CPU heavy threads available immediately to work on queries, and when the 2 background tasks can yield, the remaining 2 CPU heavy threads begin working on queries. The system response latency and CPU utilization is the same as described in the *Heavy Query Processing* configuration above.

## Code Examples (TODO)

TODO: picture of how async + CPU heavy threads interact

The standard pattern to run CPU heavy tasks from async code is:
* Launch the CPU heavy tasks on the thread pool using `spawn_with_handle`
* `await` their completion using `join_all` or some other similar `Future` combinator.

## Alternatives Considered

###  Use tokio::task::spawn_blocking for all CPU heavy work
While this definitely avoids tying up all I/O handling threads, it can also result in a large number of threads when the system is under load and has more work coming in than it can complete (aka there is no intrinsic backpressure)

### Use the main tokio::task::spawn for all  work
While this  likely results in the best possible efficiency (1:1 match with thread to core), it can mean that I/O requests (such as responding to `/ping`) are not handled in a timely manner.
