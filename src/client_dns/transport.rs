use super::ResolverSnapshot;
use crate::dns_transport::DnsTransport;
use hickory_resolver::Resolver;
use hickory_resolver::net::runtime::{RuntimeProvider, Spawn, TokioRuntimeProvider, TokioTime};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

const DNS_WORKERS: usize = 16;
const DNS_TCP_SOCKETS: usize = 32;

pub(super) type ClientResolver = Resolver<DnsTransport<TrackedRuntime>>;

struct TransportState {
    tasks: Mutex<TransportTasks>,
    tcp_slots: Arc<Semaphore>,
}

struct TransportTasks {
    open: bool,
    tasks: JoinSet<()>,
    permits: Vec<OwnedSemaphorePermit>,
}

#[derive(Clone)]
pub(super) struct TrackedRuntime {
    inner: TokioRuntimeProvider,
    state: Arc<TransportState>,
}

impl TrackedRuntime {
    fn new(tcp_slots: Arc<Semaphore>) -> Self {
        Self {
            inner: TokioRuntimeProvider::default(),
            state: Arc::new(TransportState {
                tasks: Mutex::new(TransportTasks {
                    open: false,
                    tasks: JoinSet::new(),
                    permits: Vec::new(),
                }),
                tcp_slots,
            }),
        }
    }

    fn stop(&self) -> JoinSet<()> {
        let mut state = self.state.tasks.lock().unwrap();
        state.open = false;
        let mut tasks = std::mem::take(&mut state.tasks);
        tasks.abort_all();
        tasks
    }

    fn release_permits(&self) {
        self.state.tasks.lock().unwrap().permits.clear();
    }

    fn start(&self) {
        self.state.tasks.lock().unwrap().open = true;
    }
}

impl Spawn for TrackedRuntime {
    fn spawn_bg(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        let mut state = self.state.tasks.lock().unwrap();
        if !state.open {
            return;
        }
        state.tasks.spawn(future);
        while state.tasks.try_join_next().is_some() {}
    }
}

impl RuntimeProvider for TrackedRuntime {
    type Handle = Self;
    type Timer = TokioTime;
    type Udp = <TokioRuntimeProvider as RuntimeProvider>::Udp;
    type Tcp = <TokioRuntimeProvider as RuntimeProvider>::Tcp;

    fn create_handle(&self) -> Self::Handle {
        self.clone()
    }

    fn connect_tcp(
        &self,
        server_addr: SocketAddr,
        bind_addr: Option<SocketAddr>,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn Future<Output = io::Result<Self::Tcp>> + Send>> {
        let runtime = self.clone();
        Box::pin(async move {
            let permit = Arc::clone(&runtime.state.tcp_slots)
                .acquire_owned()
                .await
                .map_err(|_| io::Error::other("客户端 DNS 连接池已关闭"))?;
            let stream = runtime
                .inner
                .connect_tcp(server_addr, bind_addr, timeout)
                .await?;
            // 后台任务实际退出、套接字关闭后才释放配额，取消清理也计入硬上限。
            let mut state = runtime.state.tasks.lock().unwrap();
            if !state.open {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "客户端 DNS 查询已结束",
                ));
            }
            state.permits.push(permit);
            Ok(stream)
        })
    }

    fn bind_udp(
        &self,
        local_addr: SocketAddr,
        server_addr: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<Self::Udp>> + Send>> {
        self.inner.bind_udp(local_addr, server_addr)
    }
}

struct DnsWorker {
    resolver: ClientResolver,
    runtime: TrackedRuntime,
    generation: Instant,
    family: Option<bool>,
}

struct WorkerPool {
    slots: Arc<Semaphore>,
    tcp_slots: Arc<Semaphore>,
    idle: Mutex<Vec<DnsWorker>>,
}

#[derive(Clone)]
pub(super) struct DnsWorkers(Arc<WorkerPool>);

impl Default for DnsWorkers {
    fn default() -> Self {
        Self(Arc::new(WorkerPool {
            slots: Arc::new(Semaphore::new(DNS_WORKERS)),
            tcp_slots: Arc::new(Semaphore::new(DNS_TCP_SOCKETS)),
            idle: Mutex::new(Vec::new()),
        }))
    }
}

impl DnsWorkers {
    #[cfg(test)]
    pub(super) fn available_slots(&self) -> (usize, usize) {
        (
            self.0.slots.available_permits(),
            self.0.tcp_slots.available_permits(),
        )
    }

    pub(super) async fn acquire(
        &self,
        snapshot: &ResolverSnapshot,
        family: Option<bool>,
    ) -> io::Result<WorkerLease> {
        let permit = Arc::clone(&self.0.slots)
            .acquire_owned()
            .await
            .map_err(|_| io::Error::other("客户端 DNS 工作池已关闭"))?;
        let idle = {
            let mut idle = self.0.idle.lock().unwrap();
            // 优先复用同地址族的工作槽，连续 TCP 连接不会因归还顺序而错过已有缓存。
            match idle.iter().rposition(|worker| worker.family == family) {
                Some(index) => Some(idle.swap_remove(index)),
                None => idle.pop(),
            }
        };
        let worker = match idle {
            Some(worker) if worker.generation == snapshot.loaded_at => worker,
            _ => {
                let runtime = TrackedRuntime::new(Arc::clone(&self.0.tcp_slots));
                let mut resolver = Resolver::builder_with_config(
                    snapshot.config.clone(),
                    DnsTransport(runtime.clone()),
                )
                .with_options(snapshot.options.clone())
                .build()
                .map_err(io::Error::other)?;
                resolver.set_hosts(Arc::clone(&snapshot.hosts));
                DnsWorker {
                    resolver,
                    runtime,
                    generation: snapshot.loaded_at,
                    family,
                }
            }
        };
        let mut worker = worker;
        worker.family = family;
        worker.runtime.start();
        Ok(WorkerLease {
            pool: Arc::clone(&self.0),
            worker: Some(worker),
            permit: Some(permit),
            stopping: None,
        })
    }
}

pub(super) struct WorkerLease {
    pool: Arc<WorkerPool>,
    worker: Option<DnsWorker>,
    permit: Option<OwnedSemaphorePermit>,
    stopping: Option<JoinSet<()>>,
}

impl WorkerLease {
    pub(super) fn resolver(&self) -> &ClientResolver {
        &self.worker.as_ref().unwrap().resolver
    }

    pub(super) async fn finish(mut self) {
        self.stopping = Some(self.worker.as_ref().unwrap().runtime.stop());
        while self.stopping.as_mut().unwrap().join_next().await.is_some() {}
        let worker = self.worker.take().unwrap();
        worker.runtime.release_permits();
        self.pool.idle.lock().unwrap().push(worker);
    }
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        let mut tasks = self
            .stopping
            .take()
            .unwrap_or_else(|| worker.runtime.stop());
        tasks.abort_all();
        let pool = Arc::clone(&self.pool);
        let permit = self.permit.take();
        // 取消任务不能异步等待清理；清理期间仍占工作槽，后台清理任务始终不超过 16 个。
        tokio::spawn(async move {
            while tasks.join_next().await.is_some() {}
            worker.runtime.release_permits();
            pool.idle.lock().unwrap().push(worker);
            drop(permit);
        });
    }
}
