#[macro_use] extern crate rocket;

use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rocket::{Rocket, Request, State, Data};
use rocket::fairing::{AdHoc, Fairing, Info, Kind};
use rocket::http::Method;
use rocket::trace::info;

struct Token(i64);

#[cfg(test)] mod tests;

#[derive(Default, Clone)]
struct Counter {
    get: Arc<AtomicUsize>,
    post: Arc<AtomicUsize>,
}

#[rocket::async_trait]
impl Fairing for Counter {
    fn info(&self) -> Info {
        Info {
            name: "GET/POST Counter",
            kind: Kind::Attach | Kind::Request
        }
    }

    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data) {
        if request.method() == Method::Get {
            self.get.fetch_add(1, Ordering::Relaxed);
        } else if request.method() == Method::Post {
            self.post.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn on_attach(&self, rocket: Rocket) -> Result<Rocket, Rocket> {
        #[get("/counts")]
        fn counts(counts: State<'_, Counter>) -> String {
            let get_count = counts.get.load(Ordering::Relaxed);
            let post_count = counts.post.load(Ordering::Relaxed);
            format!("Get: {}\nPost: {}", get_count, post_count)
        }

        Ok(rocket.manage(self.clone()).mount("/", routes![counts]))
    }
}

#[put("/")]
fn hello() -> &'static str {
    "Hello, world!"
}

#[get("/token")]
fn token(token: State<'_, Token>) -> String {
    format!("{}", token.0)
}

#[launch]
fn rocket() -> rocket::Rocket {
    rocket::ignite()
        .mount("/", routes![hello, token])
        .attach(Counter::default())
        .attach(AdHoc::on_attach("Token State", |rocket| async {
            info!("Adding token managed state...");
            match rocket.figment().extract_inner("token") {
                Ok(value) => Ok(rocket.manage(Token(value))),
                Err(_) => Err(rocket)
            }
        }))
        .attach(AdHoc::on_launch("Launch Message", |_| {
            info!("Rocket is about to launch!");
        }))
        .attach(AdHoc::on_request("PUT Rewriter", |req, _| {
            Box::pin(async move {
                info!("Incoming request: {}", req);
                if req.uri().path() == "/" {
                    info!("Changing method to `PUT`.");
                    req.set_method(Method::Put);
                }
            })
        }))
        .attach(AdHoc::on_response("Response Rewriter", |req, res| {
            Box::pin(async move {
                if req.uri().path() == "/" {
                    info!("Rewriting response body.");
                    res.set_sized_body(None, Cursor::new("Hello, fairings!"));
                }
            })
        }))
}
