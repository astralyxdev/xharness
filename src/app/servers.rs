//! Which servers can be reached: this machine's, and each remote project's.
//! Each is pinged every few seconds (`server.info`, timed). The footer shows
//! it, the sidebar marks projects that aren't connected, and a project only
//! opens while its server answers.

use std::{collections::HashMap, time::Duration};

use gpui::{Context, Task};

use crate::services::rpc;
use crate::services::server::{self, HostAddress, ServerInfo, ServerKey};

const CHECK_EVERY: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub enum Status {
    Checking,
    Connected { latency: Duration, info: ServerInfo },
    Offline(String),
    /// This machine's server, being started or stopped.
    Starting,
    Stopping,
}

impl Status {
    pub fn is_connected(&self) -> bool {
        matches!(self, Status::Connected { .. })
    }
}

/// Servers by key (`None`: this machine's).
pub struct Servers {
    statuses: HashMap<Option<ServerKey>, Status>,
    /// Remote servers to check (the remote projects').
    watched: Vec<ServerKey>,
    _poll: Task<()>,
    action: Option<Task<()>>,
}

fn ping(key: Option<&ServerKey>) -> Status {
    let address = key.cloned().map(HostAddress::Remote);
    match rpc::ping(address.as_ref()) {
        Ok((latency, info)) => Status::Connected { latency, info },
        Err(error) => Status::Offline(error),
    }
}

impl Servers {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let Ok(targets) = this.read_with(cx, |this, _| this.targets()) else {
                    break;
                };
                let results = cx
                    .background_executor()
                    .spawn(async move { targets.into_iter().map(|key| { let status = ping(key.as_ref()); (key, status) }).collect::<Vec<_>>() })
                    .await;
                if this.update(cx, |this, cx| this.apply(results, cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(CHECK_EVERY).await;
            }
        });
        Self {
            statuses: HashMap::new(),
            watched: Vec::new(),
            _poll: poll,
            action: None,
        }
    }

    fn targets(&self) -> Vec<Option<ServerKey>> {
        std::iter::once(None).chain(self.watched.iter().cloned().map(Some)).collect()
    }

    fn apply(&mut self, results: Vec<(Option<ServerKey>, Status)>, cx: &mut Context<Self>) {
        for (key, status) in results {
            // A start or stop in progress decides the local status itself.
            if key.is_none() && self.action.is_some() {
                continue;
            }
            self.statuses.insert(key, status);
        }
        cx.notify();
    }

    /// The remote servers to keep checking.
    pub fn watch(&mut self, keys: impl IntoIterator<Item = ServerKey>, cx: &mut Context<Self>) {
        let mut keys: Vec<ServerKey> = keys.into_iter().collect();
        keys.dedup();
        if keys != self.watched {
            for key in &keys {
                if !self.watched.contains(key) {
                    self.check(Some(key.clone()), cx);
                }
            }
            self.watched = keys;
        }
    }

    pub fn status(&self, key: Option<&ServerKey>) -> Status {
        self.statuses.get(&key.cloned()).cloned().unwrap_or(Status::Checking)
    }

    /// Checks a server now (Retry).
    pub fn check(&mut self, key: Option<ServerKey>, cx: &mut Context<Self>) {
        self.statuses.insert(key.clone(), Status::Checking);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let lookup = key.clone();
            let status = cx.background_executor().spawn(async move { ping(lookup.as_ref()) }).await;
            this.update(cx, |this, cx| {
                this.statuses.insert(key, status);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Starts this machine's server (and lets the app start it again when
    /// needed).
    pub fn start_local(&mut self, cx: &mut Context<Self>) {
        rpc::set_local_autostart(true);
        self.statuses.insert(None, Status::Starting);
        cx.notify();
        self.action = Some(cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move {
                    server::start_host().ok();
                    // Give it a few seconds to come up.
                    for _ in 0..50 {
                        if let status @ Status::Connected { .. } = ping(None) {
                            return status;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    ping(None)
                })
                .await;
            this.update(cx, |this, cx| {
                this.action = None;
                this.statuses.insert(None, status);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Stops this machine's server, and every shell in it. Nothing starts it
    /// again until [`Self::start_local`].
    pub fn stop_local(&mut self, cx: &mut Context<Self>) {
        rpc::set_local_autostart(false);
        self.statuses.insert(None, Status::Stopping);
        cx.notify();
        self.action = Some(cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move {
                    rpc::stop_local_server().ok();
                    for _ in 0..30 {
                        if let status @ Status::Offline(_) = ping(None) {
                            return status;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    ping(None)
                })
                .await;
            this.update(cx, |this, cx| {
                this.action = None;
                this.statuses.insert(None, status);
                cx.notify();
            })
            .ok();
        }));
    }
}
