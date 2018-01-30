use rlua::{Table, Function};
use rlua::prelude::*;
use serde_json::value::Value as JsonValue;
use std::path::Path;
use uuid::Uuid;
use indradb::{Vertex, Datastore};
use statics;
use crossbeam_channel::{Receiver, Sender, bounded};
use std::time::Duration;
use std::thread::{spawn, JoinHandle};
use super::errors;
use super::converters;

const MAPREDUCE_CHANNEL_CAPACITY: usize = 1000;
const WORKER_CHANNEL_RECV_TIMEOUT_SECONDS: u64 = 1;

pub fn create_lua_context(account_id: Uuid, arg: JsonValue) -> Result<Lua, errors::ScriptError> {
    let l = Lua::new();

    {
        let globals = l.globals();

        // Update the `package.path` to include the script root, so it's easier
        // for scripts to require each other.
        {
            let package: Table = globals.get("package")?;
            let old_path: String = package.get("path")?;
            let script_path = Path::new(&statics::SCRIPT_ROOT[..])
                .join("?.lua")
                .to_str()
                .unwrap()
                .to_string();
            package.set("path", format!("{};{}", old_path, script_path))?;
        }

        // Create a new transaction for the script
        let trans = statics::DATASTORE.transaction(account_id)?;

        // Add globals
        globals.set("trans", converters::ProxyTransaction::new(trans))?;
        globals.set("account_id", account_id.to_string())?;
        globals.set("arg", converters::JsonValue::new(arg))?;
    }

    Ok(l)
}

enum MapReduceWorkerTask {
    Map(Vertex),
    Reduce((converters::JsonValue, converters::JsonValue))
}

struct MapReduceWorker {
    thread: JoinHandle<Result<(), errors::ScriptError>>,
    shutdown_sender: Sender<()>
}

impl MapReduceWorker {
    fn start(account_id: Uuid, contents: String, path: String, arg: JsonValue, in_receiver: Receiver<MapReduceWorkerTask>, out_sender: Sender<converters::JsonValue>) -> Self {
        let (shutdown_sender, shutdown_receiver) = bounded::<()>(1);

        let thread = spawn(move || -> Result<(), errors::ScriptError> {
            let mut should_shutdown = false;
            let l = create_lua_context(account_id, arg)?;
            let table: Table = l.exec(&contents, Some(&path))?;
            let mapper: Function = table.get("map")?;
            let reducer: Function = table.get("reduce")?;

            loop {
                select_loop! {
                    recv(in_receiver, task) => {
                        let value = match task {
                            MapReduceWorkerTask::Map(vertex) => {
                                mapper.call(converters::Vertex::new(vertex))
                            },
                            MapReduceWorkerTask::Reduce((first, second)) => {
                                reducer.call((first, second))
                            }
                        }?;

                        out_sender.send(value).expect("Expected worker output channel to be open");
                    },
                    recv(shutdown_receiver, _) => {
                        should_shutdown = true;
                    },
                    timed_out(Duration::from_secs(WORKER_CHANNEL_RECV_TIMEOUT_SECONDS)) => {}
                }

                if should_shutdown {
                    return Ok(());
                }
            }
        });

        Self {
            thread: thread,
            shutdown_sender: shutdown_sender
        }
    }

    fn join(self) -> Result<(), errors::ScriptError> {
        // This ignores the error. An error should only occur if the remote
        // end of the channel disconnected, implying that the thread crashed
        // anyways.
        self.shutdown_sender.send(()).ok();
        self.thread.join().expect("Expected worker thread to not panic")
    }
}

pub struct MapReduceWorkerPool {
    thread: JoinHandle<Result<JsonValue, errors::ScriptError>>,
    in_sender: Sender<Vertex>,
    shutdown_sender: Sender<()>
}

impl MapReduceWorkerPool {
    pub fn start(account_id: Uuid, contents: String, path: String, arg: JsonValue) -> Self {
        let (mapreduce_in_sender, mapreduce_in_receiver) = bounded::<Vertex>(MAPREDUCE_CHANNEL_CAPACITY);
        let (worker_in_sender, worker_in_receiver) = bounded::<MapReduceWorkerTask>(MAPREDUCE_CHANNEL_CAPACITY);
        let (worker_out_sender, worker_out_receiver) = bounded::<converters::JsonValue>(MAPREDUCE_CHANNEL_CAPACITY);
        let (shutdown_sender, shutdown_receiver) = bounded::<()>(1);
        let mut worker_threads: Vec<MapReduceWorker> = Vec::with_capacity(*statics::MAP_REDUCE_WORKER_POOL_SIZE as usize);

        for _ in 0..*statics::MAP_REDUCE_WORKER_POOL_SIZE {
            worker_threads.push(MapReduceWorker::start(
                account_id,
                contents.clone(),
                path.clone(),
                arg.clone(),
                worker_in_receiver.clone(),
                worker_out_sender.clone()
            ));
        }

        // We cloned a bunch of copies of this, we don't need it any longer.
        // The other channels we'll continue to need.
        drop(worker_out_sender);

        let thread = spawn(move || -> Result<JsonValue, errors::ScriptError> {
            let mut should_force_shutdown = false; 
            let mut should_gracefully_shutdown = false;
            let mut pending_tasks: usize = 0;
            let mut last_reduced_item: Option<converters::JsonValue> = None;

            loop {
                select_loop! {
                    recv(mapreduce_in_receiver, vertex) => {
                        // If this errors out, all of the workers are dead
                        if worker_in_sender.send(MapReduceWorkerTask::Map(vertex)).is_err() {
                            should_force_shutdown = true;
                        } else {
                            pending_tasks += 1;
                        }
                    },
                    recv(worker_out_receiver, value) => {
                        pending_tasks -= 1;

                        if let Some(last_reduced_item_inner) = last_reduced_item {
                            // If this errors out, all of the workers are dead
                            if worker_in_sender.send(MapReduceWorkerTask::Reduce((last_reduced_item_inner, value))).is_err() {
                                should_force_shutdown = true;
                            } else {
                                pending_tasks += 1;
                            }
                            
                            last_reduced_item = None;
                        } else {
                            last_reduced_item = Some(value);
                        }
                    },
                    recv(shutdown_receiver, _) => {
                        should_gracefully_shutdown = true;
                    },
                    timed_out(Duration::from_secs(WORKER_CHANNEL_RECV_TIMEOUT_SECONDS)) => {}
                }

                // Check to see if we should shutdown
                if should_force_shutdown || (should_gracefully_shutdown && pending_tasks == 0) {
                    // Join all threads and check for any errors
                    let results: Vec<Result<(), errors::ScriptError>> = worker_threads.into_iter().map(|t| t.join()).collect();

                    for result in results.into_iter() {
                        result?;
                    }

                    // Get the final value to return
                    return Ok(match last_reduced_item {
                        // This should only happen if the graph is empty
                        None => JsonValue::Null,
                        // This should always ahppen otherwise
                        Some(value) => value.0
                    });
                }
            }
        });

        Self {
            thread: thread,
            in_sender: mapreduce_in_sender,
            shutdown_sender: shutdown_sender
        }
    }

    pub fn add_vertex(&self, vertex: Vertex) -> bool {
        self.in_sender.send(vertex).is_ok()
    }

    pub fn join(self) -> Result<JsonValue, errors::ScriptError> {
        // This ignores the error. An error should only occur if the remote
        // end of the channel disconnected, implying that the thread crashed
        // anyways.
        self.shutdown_sender.send(()).ok();
        self.thread.join().expect("Expected router thread to not panic")
    }
}
