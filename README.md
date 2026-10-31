# IconMarket

**ตลาดออนไลน์ในหมู่บ้าน.** A marketplace for one village, run entirely through a LINE Official Account. Residents buy from and sell to each other in a LINE chat, with nothing else to install, helped by an AI assistant that understands everyday Thai.

> [!WARNING]
> **Not ready to go live.** The application is built, but the pre-launch checks are still open and there is no usable release yet: `v0.1.0` predates the application. [VERIFICATION.md](VERIFICATION.md) has the state of each check and what's left.

## What it does

| Who | What they can do in LINE |
| --- | --- |
| **Buyers** | Browse by category or shop, search by keyword, or type what they want in plain Thai. Keep a cart per shop, order for pickup or delivery, pay by cash or PromptPay, follow each order's status, message the seller and rate the order |
| **Sellers** | Open a shop, list a product from a photo and a short message or through a guided flow, take pre-orders, manage stock, take each order through to hand-over, confirm payment slips and see their sales |
| **Admins** | Approve shops, review held content, suspend users, manage categories, read statistics and send announcements |

Core tasks can be done mostly by tapping, so that elderly and low-literacy users can manage. Each deployment serves one village, in Thai.

**AI is optional.** With it on, the assistant understands free-form Thai, drafts listings and helps moderate them. Voice messages, slip reading and visual search can be turned on too. It never changes data by itself: every change waits for a user's tap. With it off, everything still works through menus and buttons. Claude, Gemini, GPT and Grok are supported, chosen in the config file, with an optional second provider as a fallback.

**It doesn't move money.** IconMarket shows a PromptPay QR code for the exact amount, takes payment slips and records payment status. Sellers hand over the goods and collect payment themselves.

## How it works

```
  Villagers in the LINE app
             │
             ▼
       LINE platform
             │ ▲
     webhook │ │ reply, push
             ▼ │
      IconMarket app ─────▶ AI provider
             │
             ▼
         PostgreSQL
```

- **One process, one database.** A single Rust binary (Tokio, axum, SeaORM) and PostgreSQL. There's no broker, cache, search engine or object storage: images live in PostgreSQL, and background jobs run in-process.
- **Events are stored before they're handled.** The webhook checks LINE's signature, writes the event to an inbox table and acknowledges. A worker then handles each user's events in order, and a redelivered event takes effect only once.
- **The schema comes from the code.** Entity models generate it, and the app brings the database up to date at every start. Never change it by hand.
- **One config file holds everything,** credentials included. Environment variables are ignored.

| Route | Purpose |
| --- | --- |
| `POST /webhook/line` | Events from LINE. A bad signature gets `400` |
| `GET /health` | Whether the app is up and can reach the database |
| `GET /img/p/{id}`, `/img/s/{token}`, `/img/placeholder.png` | Product photos, expiring links to slip images, and the placeholder |
| `GET /` | Public landing page with the add-friend link and QR code |

## Run it locally

You need Rust (stable, edition 2024; 1.96 is known to work) and Docker.

```sh
docker run -d --name iconmarket-db -p 5432:5432 \
  -e POSTGRES_USER=iconmarket -e POSTGRES_PASSWORD=change-me -e POSTGRES_DB=iconmarket \
  postgres:18

cd src/IconMarket
cp config.sample.toml config.toml
cargo run
```

The first start creates the tables and default categories. Then, from another terminal:

```sh
curl localhost:8080/health
# {"status":"ok","version":"0.2.0","database":"up"}
```

The sample config starts as it is, which is enough for work that doesn't need LINE. Its LINE token is a placeholder, so the log shows `rich-menu provisioning failed, continuing`, and its AI key won't work either.

- **To chat with the bot,** you need a LINE OA and a public HTTPS address: set `app.baseUrl`, `line.channelSecret` and `line.channelAccessToken`, then connect the webhook as in [RUNBOOK §2](RUNBOOK.md#2-line-official-account) and [§10](RUNBOOK.md#10-connect-line).
- **To work without an AI key,** set `ai.provider = "mock"`, which answers by echoing your message, or `ai.enabled = false`.
- **To use another config file,** pass its path: `cargo run -- path/to/config.toml`.

Run the app from `src/IconMarket`, because it finds `static/` relative to its working directory. `config.toml` holds credentials and is git-ignored. Every key is explained in [`config.sample.toml`](src/IconMarket/config.sample.toml).

## Test

```sh
cd src/IconMarket
cargo test --no-fail-fast
cargo clippy --all-targets
```

The database tests start their own PostgreSQL 18 containers, so Docker must be running. They look at `DOCKER_HOST` and `/var/run/docker.sock`, not at your Docker context. With Colima, for example, nearly every test fails at once with `cannot start PostgreSQL` until you set:

```sh
export DOCKER_HOST="unix://$HOME/.colima/default/docker.sock"
```

**Five tests fail on a fresh checkout.** They replay real provider answers, and those recordings haven't been committed yet. Until they are, no release can be published, and a plain `cargo test` stops at those failures without running `tests/`, which is why the command above has `--no-fail-fast`. [VERIFICATION §2](VERIFICATION.md#2-blocker-the-ai-recordings) lists the tests and how to record the answers.

CI runs only on version tags, not on pull requests or `main`, so run the tests yourself before merging.

Two other tools live in the crate:

- **Load test:** `examples/loadtest` is a load generator with a fake LINE. See [VERIFICATION §12](VERIFICATION.md#12-running-it-again), and only point it at a throwaway server.
- **Rich-menu images:** `python3 assets/rich-menus/render.py` redraws them from `menus.json`. A test keeps that file in step with the code.

## Deploy

Production is one amd64 server running Debian 13, with the image `ghcr.io/two-net/iconmarket:<version>`, PostgreSQL on the same host and Caddy for HTTPS. It's designed to carry the target load of 3,000 users, 1,000 orders a day and peaks of 20 events a second on 2 vCPU and 4 GB of RAM, though that hasn't been measured on a real server yet.

The app serves plain HTTP on port 8080, so keep that port closed to the outside and put HTTPS in front of it. LINE only calls HTTPS addresses.

[RUNBOOK.md](RUNBOOK.md) covers everything from [setting up an instance](RUNBOOK.md#part-1-set-up-a-new-instance) to deploys, backups, restores, troubleshooting and monitoring. To publish a version, see [§15.3](RUNBOOK.md#153-publish-a-new-version).

## Documentation

| File | What it's for |
| --- | --- |
| [SRS.md](SRS.md) | The requirements. IDs in code comments, such as `FR-PAY-04` or `MIG-03`, refer to it |
| [RUNBOOK.md](RUNBOOK.md) | Setting up and running an instance |
| [VERIFICATION.md](VERIFICATION.md) | Pre-launch evidence: test results, load test, restore drill and what's still open |
| [config.sample.toml](src/IconMarket/config.sample.toml) | Every config key, with its default |

## Layout

The crate is in `src/IconMarket`, and its modules follow the SRS.

```
src/
  webhook        Receives LINE events: signature check and inbox
  dispatcher     Handles events in order for each user and routes their input
  users          Onboarding, consent, profile, roles and account deletion
  catalog        Shops, products, images, browsing, search and moderation
  orders         Cart, checkout, order lifecycle, ratings and buyer-seller messages
  payments       Payment status, PromptPay QR codes and slips
  notifications  Message templates, push rules and rich-menu links
  ai             Provider adapters, gateway, assistant, limits and voice
  admin          Approvals, review queue, categories, statistics and announcements
  line           LINE Messaging API client
  scheduler      In-process periodic jobs
  retention      Daily deletion of data past its retention period
  persistence    Entity models, auto-migration and seed data
  thai           Thai numbers, money, dates and phone numbers
  config         Reads and validates the config file
tests/           Tests that run the real binary
examples/        The load test
assets/          Rich-menu images and the photo placeholder
static/          The landing page
```
