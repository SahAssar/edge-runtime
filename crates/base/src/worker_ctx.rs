use crate::deno_runtime::DenoRuntime;
use crate::utils::send_event_if_event_manager_available;
use crate::utils::units::bytes_to_display;

use anyhow::{anyhow, bail, Error};
use cityhash::cityhash_1_1_1::city_hash_64;
use cpu_timer::{get_thread_time, CPUAlarmVal, CPUTimer};
use deno_core::url::Url;
use hyper::{Body, Request, Response};
use log::{debug, error};
use sb_worker_context::essentials::{
    CreateUserWorkerResult, EventWorkerRuntimeOpts, UserWorkerMsgs, WorkerContextInitOpts,
    WorkerRuntimeOpts,
};
use sb_worker_context::events::{
    BootEvent, BootFailure, LogEvent, LogLevel, PseudoEvent, UncaughtException, WorkerEvents,
};
use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub struct WorkerRequestMsg {
    pub req: Request<Body>,
    pub res_tx: oneshot::Sender<Result<Response<Body>, hyper::Error>>,
}

#[derive(Debug, Clone)]
pub struct UserWorkerProfile {
    worker_event_tx: mpsc::UnboundedSender<WorkerRequestMsg>,
    event_manager_tx: Option<mpsc::UnboundedSender<WorkerEvents>>,
}

async fn handle_request(
    unix_stream_tx: mpsc::UnboundedSender<UnixStream>,
    msg: WorkerRequestMsg,
) -> Result<(), Error> {
    // create a unix socket pair
    let (sender_stream, recv_stream) = UnixStream::pair()?;

    let _ = unix_stream_tx.send(recv_stream);

    // send the HTTP request to the worker over Unix stream
    let (mut request_sender, connection) = hyper::client::conn::handshake(sender_stream).await?;

    // spawn a task to poll the connection and drive the HTTP state
    tokio::task::spawn(async move {
        if let Err(e) = connection.without_shutdown().await {
            error!("Error in worker connection: {}", e);
        }
    });
    tokio::task::yield_now().await;

    let result = request_sender.send_request(msg.req).await;
    let _ = msg.res_tx.send(result);

    Ok(())
}

fn create_supervisor(
    key: u64,
    worker_runtime: &mut DenoRuntime,
    termination_event_tx: oneshot::Sender<WorkerEvents>,
) -> Result<CPUTimer, Error> {
    let (memory_limit_tx, mut memory_limit_rx) = mpsc::unbounded_channel::<()>();
    let thread_safe_handle = worker_runtime.js_runtime.v8_isolate().thread_safe_handle();

    // we assert supervisor is only run for user workers
    let conf = worker_runtime.conf.as_user_worker().unwrap().clone();

    worker_runtime.js_runtime.add_near_heap_limit_callback(move |cur, _| {
        debug!(
            "Low memory alert triggered: {}",
            bytes_to_display(cur as u64),
        );

        if memory_limit_tx.send(()).is_err() {
            error!("failed to send memory limit reached notification - isolate may already be terminating");
        };

        // give an allowance on current limit (until the isolate is terminated)
        // we do this so that oom won't end up killing the edge-runtime process
        cur * (conf.low_memory_multiplier as usize)
    });

    // Note: CPU timer must be started in the same thread as the worker runtime
    let (cpu_alarms_tx, mut cpu_alarms_rx) = mpsc::unbounded_channel::<()>();
    let cputimer = CPUTimer::start(conf.cpu_time_threshold_ms, CPUAlarmVal { cpu_alarms_tx })?;

    let thread_name = format!("sb-sup-{:?}", key);
    let _handle = thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();

            let future = async move {
                let mut bursts = 0;
                let mut last_burst = Instant::now();

                let sleep = tokio::time::sleep(Duration::from_millis(conf.worker_timeout_ms));
                tokio::pin!(sleep);

                loop {
                    tokio::select! {
                        Some(_) = cpu_alarms_rx.recv() => {
                            if last_burst.elapsed().as_millis() > (conf.cpu_burst_interval_ms as u128) {
                                bursts += 1;
                                last_burst = Instant::now();
                            }
                            if bursts > conf.max_cpu_bursts {
                                thread_safe_handle.terminate_execution();
                                error!("CPU time limit reached. isolate: {:?}", key);
                                return WorkerEvents::CpuTimeLimit(PseudoEvent{})
                            }
                        }

                        // wall-clock limit
                        () = &mut sleep => {
                            // use interrupt to capture the heap stats
                            //thread_safe_handle.request_interrupt(callback, std::ptr::null_mut());
                            thread_safe_handle.terminate_execution();
                            error!("wall clock duration reached. isolate: {:?}", key);
                            return WorkerEvents::WallClockTimeLimit(PseudoEvent{});

                        }

                        // memory usage
                        Some(_) = memory_limit_rx.recv() => {
                            thread_safe_handle.terminate_execution();
                            error!("memory limit reached for the worker. isolate: {:?}", key);
                            return WorkerEvents::MemoryLimit(PseudoEvent{});
                        }
                    }
                }
            };

            let result = local.block_on(&rt, future);

            // send termination reason
            let _ = termination_event_tx.send(result);
        })
        .unwrap();

    Ok(cputimer)
}

pub async fn create_worker(
    init_opts: WorkerContextInitOpts,
    event_manager_opts: Option<EventWorkerRuntimeOpts>,
) -> Result<mpsc::UnboundedSender<WorkerRequestMsg>, Error> {
    let (worker_boot_result_tx, worker_boot_result_rx) = oneshot::channel::<Result<(), Error>>();
    let (unix_stream_tx, unix_stream_rx) = mpsc::unbounded_channel::<UnixStream>();

    let (worker_key, pool_msg_tx, event_msg_tx, thread_name) = match &init_opts.conf {
        WorkerRuntimeOpts::UserWorker(worker_opts) => (
            worker_opts.key,
            worker_opts.pool_msg_tx.clone(),
            worker_opts.events_msg_tx.clone(),
            worker_opts
                .key
                .map(|k| format!("sb-iso-{:?}", k))
                .unwrap_or("isolate-worker-unknown".to_string()),
        ),
        WorkerRuntimeOpts::MainWorker(_) => (None, None, None, "main-worker".to_string()),
        WorkerRuntimeOpts::EventsWorker => (None, None, None, "events-worker".to_string()),
    };

    // spawn a thread to run the worker
    let _handle: thread::JoinHandle<Result<(), Error>> = thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let local = tokio::task::LocalSet::new();

            let mut start_time = 0;
            let result: Result<WorkerEvents, Error> = local.block_on(&runtime, async {
                match DenoRuntime::new(init_opts, event_manager_opts).await {
                    Err(err) => {
                        let _ = worker_boot_result_tx.send(Err(anyhow!("worker boot error")));
                        Ok(WorkerEvents::BootFailure(BootFailure {
                            msg: err.to_string(),
                        }))
                    }
                    Ok(mut worker_runtime) => {
                        let _ = worker_boot_result_tx.send(Ok(()));

                        let (termination_event_tx, termination_event_rx) =
                            oneshot::channel::<WorkerEvents>();
                        let _cputimer;
                        if worker_runtime.conf.is_user_worker() {
                            start_time = get_thread_time()?;

                            // cputimer is returned from supervisor and assigned here to keep it in scope.
                            _cputimer = create_supervisor(
                                worker_key.unwrap_or(0),
                                &mut worker_runtime,
                                termination_event_tx,
                            )?;
                        }

                        match worker_runtime.run(unix_stream_rx).await {
                            // if the error is execution terminated, check termination event reason
                            Err(err) => {
                                let err_string = err.to_string();
                                if err_string.ends_with("execution terminated")
                                    || err_string.ends_with("wall clock duration reached")
                                {
                                    Ok(termination_event_rx.await?)
                                } else {
                                    Ok(WorkerEvents::UncaughtException(UncaughtException {
                                        exception: err_string,
                                    }))
                                }
                            }
                            Ok(()) => Ok(WorkerEvents::EventLoopCompleted(PseudoEvent {})),
                        }
                    }
                }
            });

            match result {
                Ok(event) => {
                    send_event_if_event_manager_available(event_msg_tx.clone(), event);
                }
                Err(err) => error!("unexpected worker error {}", err),
            };

            let end_time = get_thread_time()?;
            send_event_if_event_manager_available(
                event_msg_tx.clone(),
                WorkerEvents::Log(LogEvent {
                    msg: format!("CPU time used: {:?}ms", (end_time - start_time) / 1_000_000),
                    level: LogLevel::Info,
                }),
            );

            // remove the worker from pool
            if worker_key.is_some() {
                if let Some(worker_key_unwrapped) = worker_key {
                    if let Some(tx) = pool_msg_tx {
                        let res = tx.send(UserWorkerMsgs::Shutdown(worker_key_unwrapped));
                        if res.is_err() {
                            error!(
                                "failed to send the shutdown signal to user worker pool: {:?}",
                                res.unwrap_err()
                            );
                        }
                    }
                }
            }

            Ok(())
        })
        .unwrap();

    // create an async task waiting for requests for worker
    let (worker_req_tx, mut worker_req_rx) = mpsc::unbounded_channel::<WorkerRequestMsg>();

    let worker_req_handle: tokio::task::JoinHandle<Result<(), Error>> =
        tokio::task::spawn(async move {
            while let Some(msg) = worker_req_rx.recv().await {
                let unix_stream_tx_clone = unix_stream_tx.clone();
                tokio::task::spawn(async move {
                    if let Err(err) = handle_request(unix_stream_tx_clone, msg).await {
                        error!("worker failed to handle request: {:?}", err);
                    }
                });
            }

            Ok(())
        });

    // wait for worker to be successfully booted
    let worker_boot_result = worker_boot_result_rx.await?;
    match worker_boot_result {
        Err(err) => {
            worker_req_handle.abort();
            bail!(err)
        }
        Ok(_) => Ok(worker_req_tx),
    }
}

async fn send_user_worker_request(
    worker_channel: mpsc::UnboundedSender<WorkerRequestMsg>,
    req: Request<Body>,
) -> Result<Response<Body>, Error> {
    let (res_tx, res_rx) = oneshot::channel::<Result<Response<Body>, hyper::Error>>();
    let msg = WorkerRequestMsg { req, res_tx };

    // send the message to worker
    worker_channel.send(msg)?;

    // wait for the response back from the worker
    let res = res_rx.await??;

    // send the response back to the caller

    Ok(res)
}

pub async fn create_event_worker(
    event_worker_path: Url,
    import_map_path: Option<String>,
    no_module_cache: bool,
) -> Result<mpsc::UnboundedSender<WorkerEvents>, Error> {
    let (event_tx, event_rx) = mpsc::unbounded_channel::<WorkerEvents>();

    let _ = create_worker(
        WorkerContextInitOpts {
            service_path: event_worker_path,
            no_module_cache,
            import_map_path,
            env_vars: std::env::vars().collect(),
            conf: WorkerRuntimeOpts::EventsWorker,
        },
        Some(EventWorkerRuntimeOpts { event_rx }),
    )
    .await?;

    Ok(event_tx)
}

pub async fn create_user_worker_pool(
    worker_event_sender: Option<mpsc::UnboundedSender<WorkerEvents>>,
) -> Result<mpsc::UnboundedSender<UserWorkerMsgs>, Error> {
    let (user_worker_msgs_tx, mut user_worker_msgs_rx) =
        mpsc::unbounded_channel::<UserWorkerMsgs>();

    let user_worker_msgs_tx_clone = user_worker_msgs_tx.clone();
    let _handle: tokio::task::JoinHandle<Result<(), Error>> = tokio::spawn(async move {
        let mut user_workers: HashMap<u64, UserWorkerProfile> = HashMap::new();

        loop {
            match user_worker_msgs_rx.recv().await {
                None => break,
                Some(UserWorkerMsgs::Create(mut worker_options, tx)) => {
                    let mut user_worker_rt_opts = match worker_options.conf {
                        WorkerRuntimeOpts::UserWorker(opts) => opts,
                        _ => unreachable!(),
                    };

                    // derive worker key from service path
                    // if force create is set, add current epoch mili seconds to randomize
                    let service_path = worker_options.service_path.clone();
                    let mut key_input = service_path.to_string();
                    if user_worker_rt_opts.force_create {
                        let cur_epoch_time = SystemTime::now().duration_since(UNIX_EPOCH)?;
                        key_input = format!("{}-{}", key_input, cur_epoch_time.as_millis());
                    }
                    let key = city_hash_64(key_input.as_bytes());

                    // do not recreate the worker if it already exists
                    // unless force_create option is set
                    if !user_worker_rt_opts.force_create {
                        if let Some(_worker) = user_workers.get(&key) {
                            if tx.send(Ok(CreateUserWorkerResult { key })).is_err() {
                                bail!("main worker receiver dropped")
                            }
                            continue;
                        }
                    }

                    user_worker_rt_opts.key = Some(key);
                    user_worker_rt_opts.pool_msg_tx = Some(user_worker_msgs_tx_clone.clone());
                    user_worker_rt_opts.events_msg_tx = worker_event_sender.clone();
                    worker_options.conf = WorkerRuntimeOpts::UserWorker(user_worker_rt_opts);
                    let now = Instant::now();
                    let result = create_worker(worker_options, None).await;
                    let elapsed = now.elapsed().as_secs();

                    let event_manager = worker_event_sender.clone();

                    match result {
                        Ok(user_worker_req_tx) => {
                            send_event_if_event_manager_available(
                                event_manager.clone(),
                                WorkerEvents::Boot(BootEvent {
                                    boot_time: elapsed as usize,
                                }),
                            );

                            user_workers.insert(
                                key,
                                UserWorkerProfile {
                                    worker_event_tx: user_worker_req_tx,
                                    event_manager_tx: event_manager,
                                },
                            );
                            if tx.send(Ok(CreateUserWorkerResult { key })).is_err() {
                                bail!("main worker receiver dropped")
                            };
                        }
                        Err(e) => {
                            if tx.send(Err(e)).is_err() {
                                bail!("main worker receiver dropped")
                            };
                        }
                    }
                }
                Some(UserWorkerMsgs::SendRequest(key, req, tx)) => {
                    match user_workers.get(&key) {
                        Some(worker) => {
                            let profile = worker.clone();
                            tokio::task::spawn(async move {
                                let req =
                                    send_user_worker_request(profile.worker_event_tx, req).await;
                                let result = match req {
                                    Ok(rep) => Ok(rep),
                                    Err(err) => {
                                        send_event_if_event_manager_available(
                                            profile.event_manager_tx,
                                            WorkerEvents::UncaughtException(UncaughtException {
                                                exception: err.to_string(),
                                            }),
                                        );
                                        Err(err)
                                    }
                                };

                                if tx.send(result).is_err() {
                                    error!("main worker receiver dropped")
                                }
                            });
                        }

                        None => {
                            if tx.send(Err(anyhow!("user worker not available"))).is_err() {
                                bail!("main worker receiver dropped")
                            }
                        }
                    };
                }
                Some(UserWorkerMsgs::Shutdown(key)) => {
                    user_workers.remove(&key);
                }
            }
        }

        Ok(())
    });

    Ok(user_worker_msgs_tx)
}
