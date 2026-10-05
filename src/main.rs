//! rbb — command-line entry point.

use clap::{Parser, Subcommand};
use rbb::server::{connect, migrate, serve};
use rbb::{app, config, debugbar, doctor, import, install, ops, seed, util};
use sqlx::postgres::PgPoolOptions;

#[derive(Parser)]
#[command(name = "rbb", version, about = "rbb forum server")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server (default): web, worker and scheduler roles unless --role or RBB_ROLE
    /// says otherwise. Applies pending migrations first unless RBB_MIGRATE_ON_START=false.
    Serve {
        /// Roles for this process: all, or a comma-separated list of web, worker, scheduler.
        #[arg(long)]
        role: Option<String>,
    },
    /// Run only background workers (mail delivery and outbox jobs). Same as `serve --role worker`.
    Worker,
    /// Run only scheduled tasks. Same as `serve --role scheduler`.
    Scheduler,
    /// Check a health endpoint and exit 0 if it answers 200 (for container health checks).
    Healthcheck {
        /// URL to check; default: /readyz on RBB_ADMIN_LISTEN if set, else on RBB_LISTEN.
        #[arg(long)]
        url: Option<String>,
    },
    /// Apply database migrations.
    Migrate {
        /// Don't change anything: list pending migrations, check the database matches this
        /// binary, and rehearse the pending migrations in a transaction that is rolled back
        /// (reports how long each takes and which tables it locks). Exit code 1 if `rbb
        /// migrate` would fail.
        #[arg(long)]
        check: bool,
        /// With --check: only list and compare, without the rehearsal (takes no locks).
        #[arg(long, requires = "check")]
        no_rehearse: bool,
        /// With --check: give up on a lock after this many seconds instead of waiting behind
        /// live traffic.
        #[arg(long, default_value_t = 10, requires = "check")]
        lock_timeout: u64,
    },
    /// Create default board data and an administrator account.
    Install {
        #[arg(long, default_value = "admin")]
        admin_user: String,
        #[arg(long, env = "RBB_ADMIN_PASSWORD")]
        admin_password: String,
        #[arg(long, default_value = "admin@example.com")]
        admin_email: String,
        #[arg(long, default_value = "rbb Community Forums")]
        board_name: String,
        #[arg(long, default_value = "http://localhost:8080")]
        board_url: String,
    },
    /// Generate a large synthetic board for load testing.
    Seed {
        #[arg(long, default_value_t = 10_000)]
        users: i64,
        #[arg(long, default_value_t = 50_000)]
        threads: i64,
        #[arg(long, default_value_t = 1_000_000)]
        posts: i64,
    },
    /// Check configuration, database, board and plugins, and explain how to fix problems.
    Doctor {
        /// Treat warnings as failures (non-zero exit code).
        #[arg(long)]
        strict: bool,
    },
    /// Recount all denormalized counters.
    Recount,
    /// Verify denormalized counters against the data (exit code 1 on mismatch).
    Check,
    /// Import a MyBB database (MySQL dump converted, or direct connection) — see docs.
    ImportMybb {
        /// MySQL connection URL of the MyBB database, e.g. mysql://user:pass@host/mybb
        #[arg(long)]
        mysql_url: String,
        #[arg(long, default_value = "mybb_")]
        prefix: String,
        /// Where MyBB's uploads/ directory was copied, relative to RBB_UPLOAD_DIR.
        #[arg(long, default_value = "mybb")]
        uploads_prefix: String,
        /// Confirm that all existing users, forums and posts in the rbb database will be replaced.
        #[arg(long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let json_logs = std::env::var("RBB_LOG_JSON")
        .map(|v| v == "true")
        .unwrap_or(false);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "rbb=info,tower_http=warn,sqlx=warn".into());
    use tracing_subscriber::prelude::*;
    let fmt_layer = if json_logs {
        tracing_subscriber::fmt::layer()
            .json()
            .with_filter(filter)
            .boxed()
    } else {
        tracing_subscriber::fmt::layer().with_filter(filter).boxed()
    };
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(debugbar::QueryLayer.with_filter(debugbar::filter()))
        .init();
    let cli = Cli::parse();
    // Before loading the configuration: the health check must work with a minimal environment.
    if let Some(Cmd::Healthcheck { url }) = &cli.cmd {
        let url = url.clone().unwrap_or_else(|| {
            let addr = std::env::var("RBB_ADMIN_LISTEN")
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| std::env::var("RBB_LISTEN").ok())
                .unwrap_or_else(|| config::DEFAULT_LISTEN.to_string())
                .replace("0.0.0.0", "127.0.0.1")
                .replace("[::]", "[::1]");
            format!("http://{addr}/readyz")
        });
        std::process::exit(rbb::infra::observe::healthcheck(&url).await);
    }
    // Before loading the configuration: doctor reports configuration problems instead of exiting.
    if let Some(Cmd::Doctor { strict }) = cli.cmd {
        std::process::exit(doctor::run(strict).await);
    }
    let mut cfg = config::Config::from_env()?;
    match cli.cmd.unwrap_or(Cmd::Serve { role: None }) {
        Cmd::Serve { role } => {
            if let Some(r) = role {
                cfg.roles = config::Roles::parse(&r)?;
            }
            serve(cfg).await
        }
        Cmd::Worker => {
            cfg.roles = config::Roles::parse("worker")?;
            serve(cfg).await
        }
        Cmd::Scheduler => {
            cfg.roles = config::Roles::parse("scheduler")?;
            serve(cfg).await
        }
        Cmd::Healthcheck { .. } => unreachable!("handled before the configuration is loaded"),
        Cmd::Migrate {
            check: true,
            no_rehearse,
            lock_timeout,
        } => {
            let db = connect(&cfg).await?;
            let migrator = sqlx::migrate!("./migrations");
            let migrations: Vec<_> = migrator.iter().cloned().collect();
            let report = rbb::infra::migrations::check(
                &db,
                &migrations,
                !no_rehearse,
                std::time::Duration::from_secs(lock_timeout),
            )
            .await?;
            print!("{}", rbb::infra::migrations::render(&report, !no_rehearse));
            if !report.ok() {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Migrate { .. } => {
            let db = connect(&cfg).await?;
            migrate(&db).await?;
            println!("migrations applied");
            Ok(())
        }
        Cmd::Install {
            admin_user,
            admin_password,
            admin_email,
            board_name,
            board_url,
        } => {
            if admin_password.chars().count() < 10 {
                anyhow::bail!("the administrator password must be at least 10 characters long");
            }
            let db = connect(&cfg).await?;
            migrate(&db).await?;
            if rbb::server::is_installed(&db).await? {
                anyhow::bail!(
                    "this database already holds an installed board; nothing was changed"
                );
            }
            install::install(
                &db,
                &admin_user,
                &admin_password,
                &admin_email,
                &board_name,
                &board_url,
            )
            .await?;
            println!("board installed; administrator '{admin_user}' created");
            Ok(())
        }
        Cmd::Seed {
            users,
            threads,
            posts,
        } => {
            let db = connect(&cfg).await?;
            migrate(&db).await?;
            seed::seed(&db, users, threads, posts).await
        }
        Cmd::Recount => {
            let db = connect(&cfg).await?;
            let app = app::AppState::new(cfg, db).await?;
            ops::rebuild_all_counters(&app)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("counters rebuilt");
            Ok(())
        }
        Cmd::Doctor { .. } => unreachable!("handled before the configuration is loaded"),
        Cmd::Check => {
            let db = connect(&cfg).await?;
            let problems = ops::check_counters(&db).await?;
            if problems.is_empty() {
                println!("all counters are consistent");
                Ok(())
            } else {
                for p in &problems {
                    println!("{p}");
                }
                std::process::exit(1)
            }
        }
        Cmd::ImportMybb {
            mysql_url,
            prefix,
            uploads_prefix,
            yes,
        } => {
            if !yes {
                anyhow::bail!(
                    "importing replaces all users, forums and posts in this database; re-run with --yes to continue"
                );
            }
            let db = connect(&cfg).await?;
            migrate(&db).await?;
            if sqlx::query_scalar::<_, i32>("SELECT gid FROM usergroups LIMIT 1")
                .fetch_optional(&db)
                .await?
                .is_none()
            {
                install::install(
                    &db,
                    "admin",
                    &util::random_token(16),
                    "admin@example.com",
                    "rbb Community Forums",
                    "http://localhost:8080",
                )
                .await?;
            }
            // Load with foreign-key triggers off (when permitted); orphans are repaired afterwards.
            let is_super: bool =
                sqlx::query_scalar("SELECT rolsuper FROM pg_roles WHERE rolname = current_user")
                    .fetch_one(&db)
                    .await?;
            let load_db = if is_super {
                PgPoolOptions::new()
                    .max_connections(2)
                    .after_connect(|c, _| {
                        Box::pin(async move {
                            sqlx::query("SET session_replication_role = replica")
                                .execute(c)
                                .await?;
                            Ok(())
                        })
                    })
                    .connect(&cfg.database_url)
                    .await?
            } else {
                db.clone()
            };
            let mut imp = import::Importer::connect(&mysql_url, load_db.clone(), &prefix).await?;
            imp.run(&uploads_prefix).await?;
            load_db.close().await;
            install::upgrade(&db).await?;
            println!("rebuilding counters…");
            let app = app::AppState::new(cfg, db).await?;
            ops::rebuild_all_counters(&app)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            app.bump_parser_rev().await?;
            println!("import complete. MyBB passwords work as-is and are upgraded on first login.");
            Ok(())
        }
    }
}
