use lazy_static::lazy_static;
use rand::Rng;
use std::sync::{Arc, Mutex};
use std::{thread, time};

#[derive(Debug)]
struct Server {
    name: String,
}

impl Server {
    fn new<S: Into<String>>(name: S) -> Self {
        Server { name: name.into() }
    }

    fn respond<S: Into<String>>(&self, response: S) {
        println!("[{}]: {}", self.name, response.into());
    }
}

impl Drop for Server {
    fn drop(&mut self) {}
}

#[derive(Debug)]
struct ServerPool {
    servers: Vec<Mutex<Server>>,
}

impl ServerPool {
    fn new(size: usize) -> Self {
        let mut servers = vec![];

        for i in 0..size {
            let server = Server::new(format!("{}", i));
            servers.push(Mutex::new(server));
        }

        ServerPool { servers }
    }
}

lazy_static! {
    static ref POOL_RC: Arc<ServerPool> = Arc::new(ServerPool::new(5));
}

thread_local!(
    static LOCAL_POOL: Arc<ServerPool> = POOL_RC.clone();
);

fn main() {
    for i in 0..10 {
        thread::spawn(move || {
            let mut rng = rand::thread_rng();
            let ms = rng.gen_range(0..1);
            thread::sleep(time::Duration::from_millis(ms));

            LOCAL_POOL.with(|pool| {
                let server = pool
                    .servers
                    .iter()
                    .find_map(|server| server.try_lock().ok())
                    .or_else(|| pool.servers[0].lock().ok())
                    .unwrap();

                server.respond(format!("{} a", i));
                server.respond(format!("{} b", i));
            });
        });
    }

    thread::sleep(time::Duration::from_millis(5));
}
