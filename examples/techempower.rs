use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use may_minihttp::{BodyWriter, HttpService, HttpServiceFactory, Request, Response};
use may_postgres::{self, Client, Statement};
use oorandom::Rand32;
use smallvec::SmallVec;

use serde_derive::Serialize;

#[derive(Serialize)]
struct HeloMessage {
    message: &'static str,
}

#[derive(Serialize)]
struct WorldRow {
    id: i32,
    randomnumber: i32,
}

struct PgConnectionPool {
    idx: AtomicUsize,
    clients: Vec<Arc<PgConnection>>,
}

impl PgConnectionPool {
    fn new(db_url: &str, size: usize) -> PgConnectionPool {
        let mut clients = Vec::with_capacity(size);
        for _ in 0..size {
            let client = PgConnection::new(db_url);
            clients.push(Arc::new(client));
        }

        PgConnectionPool {
            idx: AtomicUsize::new(0),
            clients,
        }
    }

    fn get_connection(&self) -> (Arc<PgConnection>, usize) {
        let idx = self.idx.fetch_add(1, Ordering::Relaxed);
        let len = self.clients.len();
        (self.clients[idx % len].clone(), idx)
    }
}

struct PgConnection {
    client: Client,
    world: Statement,
}

unsafe impl Send for PgConnection {}

impl PgConnection {
    fn new(db_url: &str) -> Self {
        let client = may_postgres::connect(db_url).unwrap();
        let world = client
            .prepare("SELECT id, randomnumber FROM world WHERE id=$1")
            .unwrap();

        PgConnection { client, world }
    }

    fn get_world(&self, random_id: i32) -> Result<WorldRow, may_postgres::Error> {
        let row = self.client.query_one(&self.world, &[&random_id])?;
        Ok(WorldRow {
            id: row.get(0),
            randomnumber: row.get(1),
        })
    }

    fn get_worlds(
        &self,
        num: usize,
        rand: &mut Rand32,
    ) -> Result<Vec<WorldRow>, may_postgres::Error> {
        let mut queries = SmallVec::<[may_postgres::RowStream; 32]>::new();
        // let mut queries = Vec::with_capacity(num);
        for _ in 0..num {
            let random_id = rand.rand_range(1..10001) as i32;
            queries.push(
                self.client
                    .query_raw(&self.world, utils::slice_iter(&[&random_id]))?,
            );
        }

        let mut worlds = Vec::with_capacity(num);
        for mut q in queries {
            match q.next().transpose()? {
                Some(row) => worlds.push(WorldRow {
                    id: row.get(0),
                    randomnumber: row.get(1),
                }),
                None => unreachable!(),
            }
        }
        Ok(worlds)
    }
}

struct Techempower {
    db: Arc<PgConnection>,
    rng: Rand32,
}

impl HttpService for Techempower {
    fn call(&mut self, req: Request, rsp: &mut Response) -> io::Result<()> {
        // Bare-bones router
        match req.path() {
            "/json" => {
                rsp.header("Content-Type: application/json");
                serde_json::to_writer(
                    BodyWriter(rsp.body_mut()),
                    &HeloMessage {
                        message: "Hello, World",
                    },
                )?;
            }
            "/plaintext" => {
                rsp.header("Content-Type: text/plain").body("Hello, World!");
            }
            "/db" => {
                let random_id = self.rng.rand_range(1..10001) as i32;
                let world = self.db.get_world(random_id).unwrap();
                rsp.header("Content-Type: application/json");
                serde_json::to_writer(BodyWriter(rsp.body_mut()), &world)?;
            }
            p if p.starts_with("/queries") => {
                let q = utils::get_query_param(p) as usize;
                let worlds = self.db.get_worlds(q, &mut self.rng).unwrap();
                rsp.header("Content-Type: application/json");
                serde_json::to_writer(BodyWriter(rsp.body_mut()), &worlds)?;
            }
            _ => {
                rsp.status_code("404", "Not Found");
            }
        }

        Ok(())
    }
}

struct HttpServer {
    db_pool: PgConnectionPool,
}

impl HttpServiceFactory for HttpServer {
    type Service = Techempower;

    fn new_service(&self) -> Self::Service {
        let (db, idx) = self.db_pool.get_connection();
        let rng = Rand32::new(idx as u64);
        Techempower { db, rng }
    }
}

fn main() {
    let cpus = num_cpus::get();
    may::config()
        .set_io_workers(cpus)
        .set_workers(cpus)
        .set_pool_capacity(10000);
    let db_url = "postgres://benchmarkdbuser:benchmarkdbpass@127.0.0.1/hello_world";
    let http_server = HttpServer {
        db_pool: PgConnectionPool::new(db_url, cpus),
    };
    let server = http_server.start("127.0.0.1:8081").unwrap();
    server.join().unwrap();
}

mod utils {
    use std::cmp;
    pub fn get_query_param(query: &str) -> u16 {
        let q = if let Some(pos) = query.find("?q") {
            query.split_at(pos + 3).1.parse::<u16>().ok().unwrap_or(1)
        } else {
            1
        };
        cmp::min(500, cmp::max(1, q))
    }

    pub fn slice_iter<'a>(
        s: &'a [&'a (dyn may_postgres::ToSql + Sync)],
    ) -> impl ExactSizeIterator<Item = &'a dyn may_postgres::ToSql> + 'a {
        s.iter().map(|s| *s as _)
    }
}
