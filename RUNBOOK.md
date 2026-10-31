# IconMarket Operator Runbook

How to set up and run one IconMarket instance. It is for the operator: the person who runs the server and edits the config file. Requirement IDs refer to [SRS.md](SRS.md).

- [Part 1: Set up a new instance](#part-1-set-up-a-new-instance), in order. Each step needs the one before it.
- [Part 2: Run it](#part-2-run-it): admins, config changes, deploys, backups, restores, troubleshooting and monitoring.

Replace these placeholders wherever they appear:

| Placeholder | Meaning | Example |
| --- | --- | --- |
| `<host>` | Public domain name of the server | `market.example.com` |
| `<version>` | Release to run: the newest of the [tags](https://github.com/two-net/iconmarket/tags). Always pinned, never `latest` | `v0.2.0` |
| `<oa-id>` | Your OA's basic ID, without the `@` ([§2.2](#22-copy-the-two-credentials)) | `123abcde` |
| `<running-version>`, `<previous-version>` | The release running now, and the one before it ([§15.1](#151-deploy-a-new-version)) | `v0.2.0` |
| `<date>` | The date and time in a backup's file name | `20261031-020000` |
| `<user_id>` | A number from a log line ([§16.5](#165-what-a-restore-loses)) | `42` |

LINE changes its consoles and prices (A-07). If a label here doesn't match what you see, follow [LINE's Messaging API docs](https://developers.line.biz/en/docs/messaging-api/). The details here were checked in October 2026.

# Part 1: Set up a new instance

## 1. Before you start

You need:

| Item | Cost | Notes |
| --- | --- | --- |
| A LINE Official Account | Free to create; plan chosen in [§2.3](#23-choose-a-plan) | |
| An AI provider account | Pay per use | Optional: IconMarket works without AI ([§3](#3-ai-provider-key)) |
| One x86-64 (amd64) server running Debian 13 | Monthly rent | 2 vCPU and 4 GB RAM handle the full target load (NFR-CAP-02). **Not ARM:** the image is built for amd64 only |
| A domain name | Yearly fee | Point it at the server ([§4](#4-server)) |
| A second computer for backup copies | One you already own | Linux, macOS or WSL, and on when the copy runs ([§16.2](#162-copy-off-the-server)) |

Apart from the domain name, which a public HTTPS address needs, these are the only running costs: one server, one PostgreSQL database (on that server), the LINE OA plan and the AI provider (NFR-COST-03). The restore test also needs a temporary server for a few hours ([§12](#12-go-live-checklist)). Everything else used here is free: Docker, PostgreSQL, Caddy and Let's Encrypt.

**Disk:** start with at least 40 GB on the server. It holds the system, Docker images, logs, the database and 7 backups, each about as large as the database because its images don't compress further. At the full target load the database is about 20 GB (NFR-CAP-03), so plan for about 200 GB. The second computer needs room for the 7 backups.

Allow an afternoon. LINE's Business ID registration and DNS changes may take longer.

## 2. LINE Official Account

### 2.1 Create the account

1. Register for a Business ID and create the Official Account: follow steps 1-1 to 1-3 of [LINE's getting-started guide](https://developers.line.biz/en/docs/messaging-api/getting-started/). Messaging API channels can't be created directly in the LINE Developers Console any more; the account comes first.
2. Sign in to [LINE Official Account Manager](https://manager.line.biz/) in a web browser. Open **Settings**, then **Messaging API**, and enable the Messaging API (A-01).
3. Choose a provider when asked. **This is permanent:** LINE doesn't let you change it later. Pick one you will keep, because LINE user IDs differ per provider, so a different provider means different IDs for everyone.

### 2.2 Copy the two credentials

Sign in to the [LINE Developers Console](https://developers.line.biz/console/) with the same account, then open your provider and channel. Keep both values somewhere private until [§7](#7-config-file).

| Config key | Where to find it |
| --- | --- |
| `line.channelSecret` | **Basic settings** tab, **Channel secret** |
| `line.channelAccessToken` | **Messaging API** tab, under the channel access token section: issue a **long-lived** token |

Also note your OA's basic ID for [§8](#8-start-script): the `@` ID shown on the **Messaging API** tab as **Bot basic ID**, and in OA Manager.

Use the long-lived token, not the v2.1 token LINE's guide recommends. IconMarket reads one fixed token from the config file and doesn't renew it, and the other token types expire within 30 days. Long-lived tokens don't expire.

Reissuing either value invalidates the old one, so IconMarket stops working until the config file has the new value and the app has restarted.

### 2.3 Choose a plan

LINE counts push messages against a monthly quota, once per recipient. Replies to a user's message are free (A-03). Estimate your pushes per month:

| What happens | Pushes |
| --- | --- |
| One order, cash | About 4: new order to the seller; accepted, ready or delivering, and completed to the buyer |
| One order, PromptPay | About 5: as above, plus the slip to the seller |
| One announcement | 1 per follower |
| Daily sales summary, off by default | 1 per seller with orders that day |
| Group digest, off by default | 1 per group member, every day |

Then compare with the plans. These are the Thai plans as of October 2026, before 7% VAT. Check [LINE's plan page](https://lineforbusiness.com/th/service/line-oa-features/) for current figures.

| Plan | Fee per month | Messages per month | Extra messages |
| --- | --- | --- | --- |
| ฟรี (Free) | 0 THB | 300 | None listed |
| เบสิก (Basic) | 1,280 THB | 15,000 | 0.10 THB each |
| โปร (Pro) | 1,780 THB | 35,000 | 0.06 THB each |

Examples:

- 2 orders a day is about 300 pushes a month, the whole Free plan. One announcement to 300 followers uses the same.
- 20 PromptPay orders a day is 3,000 pushes a month, which fits Basic with room for announcements.
- The full target of 1,000 orders a day (NFR-CAP-01) is about 150,000 pushes a month. If the quota gets tight, turn off the optional pushes in the `[notification]` section.

Start small and change plan when needed. To see usage, an admin taps **สถิติ** in the bot, which shows pushes sent and given up this month. When the quota runs out, replies still work but pushes fail until next month ([§18](#18-troubleshooting)). Buy or change the plan in OA Manager on the web; LINE doesn't sell monthly plans through iOS.

### 2.4 Turn off LINE's own replies

IconMarket sends its own messages, so LINE's automatic ones must be off (SRS §2.4).

1. In the LINE Developers Console, open the **Messaging API** tab.
2. Under **Greeting messages**, click **Edit**. This opens OA Manager (**Settings**, then **Response settings**). Set greeting messages to disabled.
3. Do the same for **Auto-reply messages**.

The webhook is set up later, in [§10](#10-connect-line), once the server is answering.

## 3. AI provider key

Pick one provider and create an API key (A-02). Each bills you for what you use, so set up its billing before go-live. A key is often shown only once, so copy it straight away.

| Provider | `ai.provider` | Where to create a key | Notes |
| --- | --- | --- | --- |
| Anthropic | `claude` | [Keys page](https://platform.claude.com/settings/keys) | |
| Google | `gemini` | [AI Studio](https://aistudio.google.com/apikey) | The only one that takes voice messages. Prepaid accounts: at a zero balance every key stops working |
| OpenAI | `gpt` | Dashboard, see the [quickstart](https://developers.openai.com/api/docs/quickstart) | |
| xAI | `grok` | [API keys page](https://console.x.ai/team/default/api-keys) | Load credits first |

Before choosing, see the `[ai]` section of the [sample config](https://github.com/two-net/iconmarket/blob/main/src/IconMarket/config.sample.toml), which lists what each provider can do. Take the model name from the provider's own documentation: the sample's examples may have dated.

Changing provider later takes only a config change and a restart (FR-AIP-03), plus a new privacy notice version ([§14](#14-config-changes)). A second provider as a fallback is optional, and uses the `[ai.fallback]` keys.

To run without AI, set `ai.enabled = false` in [§7](#7-config-file) and skip this step. Everything still works through menus and buttons.

## 4. Server

Run every command in this runbook as root on the server, unless it says otherwise. Log in as root, or run `sudo -i`.

**Point the domain at the server:** create a DNS A record from `<host>` to the server's public IP address. Delete any AAAA record for `<host>` that points elsewhere, or the certificate can fail. From your own computer, wait until `nslookup <host>` shows that address.

**Install the software:**

```sh
apt-get update
apt-get install -y sudo curl rsync cron nano ufw docker.io postgresql caddy
```

**Keep logs across reboots:**

```sh
mkdir -p /var/log/journal
systemctl restart systemd-journald
```

**Close everything but SSH and the web.** The app serves plain HTTP on port 8080 on all network interfaces, so that port must never be reachable from outside:

```sh
ufw allow 22/tcp
ufw allow 80/tcp
ufw allow 443/tcp
ufw enable
```

If SSH isn't on port 22, allow its port instead, or `ufw enable` locks you out. If your hosting provider has its own firewall, open only the same three ports there. Leave port 80 open: Caddy needs it to get and renew the certificate.

## 5. PostgreSQL

IconMarket creates and updates its own tables at every start, so its database user must own the database (MIG-08). The database must also be UTF-8, or the app refuses to start.

Create a password, then the user and the database. Paste the password when asked for it, and keep it for [§7](#7-config-file):

```sh
openssl rand -hex 24
sudo -u postgres createuser --pwprompt iconmarket
sudo -u postgres createdb --owner=iconmarket --encoding=UTF8 --template=template0 iconmarket
```

Don't create any tables. The app does that on first start.

## 6. HTTPS

LINE only calls an HTTPS address with a certificate from a trusted authority, over TLS 1.2 or later (SRS §2.4). Self-signed certificates are refused. Caddy gets a free certificate from Let's Encrypt and renews it by itself.

Replace the contents of `/etc/caddy/Caddyfile` with:

```
<host> {
	reverse_proxy localhost:8080
}
```

Send every path to the app. Don't limit it to the paths in SRS §3.4: the app also serves `/` and `/img/placeholder.png`.

```sh
systemctl reload caddy
curl -sS -o /dev/null -w '%{http_code}\n' https://<host>/
```

`502` is right at this point: the certificate works and the app isn't running yet. The first certificate can take a minute, so retry before looking further. If the certificate error stays, DNS or ports 80 and 443 aren't ready. See `journalctl -u caddy -n 30`.

## 7. Config file

All of the app's configuration, credentials included, lives in this one file (CFG-01). Environment variables are ignored.

```sh
mkdir -p /etc/iconmarket
curl -fsSL https://raw.githubusercontent.com/two-net/iconmarket/<version>/src/IconMarket/config.sample.toml -o /etc/iconmarket/config.toml
chown 10001:10001 /etc/iconmarket/config.toml
chmod 600 /etc/iconmarket/config.toml
```

Uid 10001 is the service account the container runs as. Mode 600 means only it, and root, can read the file (CFG-09). It holds credentials in plain text, so never commit it, paste it into a chat or send it by email.

Edit the file with `nano /etc/iconmarket/config.toml`. Every key has a comment. These must be changed:

| Key | Set it to |
| --- | --- |
| `app.baseUrl` | `https://<host>` |
| `village.name` | Your village's name |
| `line.channelSecret`, `line.channelAccessToken` | The values from [§2.2](#22-copy-the-two-credentials) |
| `ai.provider`, `ai.model`, `ai.apiKey` | The values from [§3](#3-ai-provider-key). Without AI: set `ai.enabled = false` and set `model` and `apiKey` to `""` |
| `database.password` | The password from [§5](#5-postgresql) |
| `privacy.noticeText` | Who runs the market and how to reach them, at most 3000 characters. The system adds the rest of the notice (NFR-PRIV-02) |

**The app can't detect a value you forgot.** `change-me` is a valid string as far as it knows. Check with:

```sh
grep -n change-me /etc/iconmarket/config.toml
```

That must print nothing. `admin.lineUserIds` keeps its placeholder, `U00000000000000000000000000000000`, which the `grep` doesn't match; [§11](#11-first-admin) replaces it. Leave `database.host` as `localhost` and `logging.file` empty.

These are optional, empty by default, and can change at any time: `village.zones` (areas offered to buyers), `village.adminContact` (contact text in help) and `village.joinCode` (a code residents must enter to register).

## 8. Start script

This script starts a given version and replaces whatever is running, so a first start, a deploy and a rollback are all the same command. It also fills in the public page at `https://<host>/`. That page ships inside the image pointing at a placeholder account, so without this step the add-friend buttons and QR code would send villagers to the wrong place.

Create `/usr/local/bin/iconmarket-start` with this content, and fill in the four settings near the top. Avoid `'`, `|`, `&` and `\` in them:

```sh
#!/bin/sh
# Usage: iconmarket-start <version>
set -eu

# The public page. Use your own add-friend link, or https://line.me/R/ti/p/@<oa-id>
line_url='https://line.me/R/ti/p/@<oa-id>'
line_id='@<oa-id>'
privacy_url=''   # Link to the full privacy notice; empty links to the page's own section
estate_name=''   # Shown after "ตลาดของลูกบ้าน"; empty hides it

image="ghcr.io/two-net/iconmarket:$1"
page=/etc/iconmarket/index.html

docker pull "$image"
docker run --rm --entrypoint cat "$image" static/index.html > "$page"
sed -i \
  -e "s|lineUrl: '[^']*'|lineUrl: '$line_url'|" \
  -e "s|lineId: '[^']*'|lineId: '$line_id'|" \
  -e "s|privacyUrl: '[^']*'|privacyUrl: '$privacy_url'|" \
  -e "s|estateName: '[^']*'|estateName: '$estate_name'|" \
  "$page"

docker stop iconmarket 2>/dev/null || true
docker rm iconmarket 2>/dev/null || true
docker run -d --name iconmarket \
  --restart unless-stopped \
  --stop-timeout 15 \
  --network host \
  --log-driver journald \
  -v /etc/iconmarket/config.toml:/app/config.toml:ro \
  -v "$page":/app/static/index.html:ro \
  "$image"

iconmarket-wait
```

Create `/usr/local/bin/iconmarket-wait` too. A start or restart returns before the app is ready, so this waits until it answers:

```sh
#!/bin/sh
# Waits up to a minute for the app to answer, then prints its health.
for i in $(seq 60); do
  curl -fs localhost:8080/health && { echo; exit 0; }
  sleep 1
done
echo 'No answer after a minute. See: docker logs iconmarket' >&2
exit 1
```

Make both executable:

```sh
chmod 755 /usr/local/bin/iconmarket-start /usr/local/bin/iconmarket-wait
```

To change the four settings later, edit the script and run `iconmarket-start <running-version>`. `docker restart` doesn't apply them.

## 9. Start the app

```sh
iconmarket-start <version>
docker logs iconmarket
```

The first start creates all the tables and the default categories. It ends by printing `{"status":"ok",...}`. Logs are JSON, one object per line. Among them you should see these messages, in this order:

```
connected to the database
database schema is up to date
seed data applied
accepting requests
```

A `rich-menu provisioning failed, continuing` warning means the access token is wrong. Anything else that stops startup is covered in [§18](#18-troubleshooting). On later starts, lines like `relation "..." already exists, skipping` are normal.

The app comes back by itself after a reboot. If it starts before PostgreSQL, it logs `cannot connect to the database` and retries.

Then check from your own computer:

```sh
curl -fsS https://<host>/health
```

It should print `{"status":"ok","version":"...","database":"up"}`. Open `https://<host>/` in a browser too, and check that the add-friend button opens your OA.

## 10. Connect LINE

In the LINE Developers Console, on the **Messaging API** tab:

1. Under **Webhook URL**, click **Edit**, enter `https://<host>/webhook/line`, and click **Update**.
2. Click **Verify**. It must show **Success**.
3. Turn on **Use webhook**.
4. Turn on **Webhook redelivery**. It's off by default. It makes LINE resend events that arrive while the app is restarting.
5. Turn on **Error statistics aggregation**. Failed deliveries then appear on the **Webhook errors** tab.

Add your OA as a friend in LINE and send it `สวัสดี`. You should get IconMarket's privacy notice and nothing from LINE's own auto-reply.

## 11. First admin

The config file must list an admin, but a LINE user ID can only be found through a running instance. That's why [§7](#7-config-file) left the placeholder in.

1. In LINE, send `#myid` to the OA. This works before you accept the privacy notice. You get two messages, and the second is your ID: `U` followed by 32 hex digits. Press and hold to copy it.
2. In `/etc/iconmarket/config.toml`, replace the placeholder:

   ```toml
   lineUserIds = ["U0123456789abcdef0123456789abcdef"]
   ```

3. Restart and check:

   ```sh
   docker restart iconmarket
   iconmarket-wait
   docker logs iconmarket 2>&1 | grep 'seed data applied' | tail -n 1
   ```

   The line should show `"admins_granted":1`. It counts only that start's changes, so `0` on later restarts is normal.

Accept the privacy notice in LINE. Your rich menu now has an admin tab.

## 12. Go-live checklist

First set up backups: do [§16.1](#161-nightly-dump) and [§16.2](#162-copy-off-the-server) now. Then go through this list.

- [ ] `grep -n change-me /etc/iconmarket/config.toml` prints nothing, and the placeholder admin ID is gone.
- [ ] `ls -l /etc/iconmarket/config.toml` shows `-rw-------` and owner `10001`.
- [ ] The config file isn't in any git repository.
- [ ] `curl -fsS https://<host>/health` shows `"status":"ok"`.
- [ ] From another network, `curl -m 5 http://<host>:8080/health` fails. If it answers, the firewall is open.
- [ ] **Verify** shows **Success**, and **Use webhook** and **Webhook redelivery** are on.
- [ ] Someone else adds the OA and sends a message: they get the privacy notice, with no greeting or auto-reply from LINE.
- [ ] Your rich menu has the admin tab.
- [ ] `https://<host>/` opens your OA, not another one.
- [ ] Backups run, and a copy reached the second computer ([§16](#16-backup-and-restore)).
- [ ] An uptime monitor checks `/health`, and its alert has reached you in a test ([§19](#19-monitoring)).
- [ ] **A restore has been tested and timed** against the 4-hour target (NFR-REL-07). Do this before villagers join: rent a temporary server for a few hours, point `<host>` at it, and follow the new-server steps in [§16.4](#164-restore). Check that it works, then point `<host>` back and delete the temporary server. Finally, on the second computer, run `ssh-keygen -R <host>` and `copy.sh` once by hand, or the scheduled copies will stop on the changed host key.

Then share the QR code or the `@<oa-id>` ID with the village (A-04): the page at `https://<host>/` shows a QR code, and OA Manager has the account's own. Posters and the village's existing chat groups work well. Set `village.joinCode` first if only residents should register, and give them the code the same way.

# Part 2: Run it

## 13. Admins

`admin.lineUserIds` is the only list of admins. There is no way to change it from the chat.

**To add one:** have them add the OA as a friend and send `#myid`. Add the ID they send you to the list, then run `docker restart iconmarket && iconmarket-wait`. The ID must come from this OA, because IDs differ between LINE providers.

```toml
lineUserIds = ["U0123456789abcdef0123456789abcdef", "Ufedcba9876543210fedcba9876543210"]
```

**To remove one:** delete the ID and restart. At every start, anyone not on the list loses the admin role, and their rich menu changes to match. `admins_granted` and `admins_revoked` in the `seed data applied` log line show what changed.

## 14. Config changes

Edit `/etc/iconmarket/config.toml`, then:

```sh
docker restart iconmarket
iconmarket-wait
```

Changes only take effect on restart (CFG-08). If the file has mistakes, the app lists them all and doesn't start: `iconmarket-wait` reports no answer, and `docker logs --tail 20 iconmarket` shows the list. A warning `unknown config key, ignored` means a typo in a key name.

**Change `privacy.noticeVersion` as well** when you change anything the notice states: `privacy.noticeText`, `privacy.noticeUrl`, `ai.enabled`, `ai.provider`, `ai.fallback.provider`, `ai.features.voice`, `payment.methods`, `retention.aiMessageDays` or `retention.slipDays`. A new version asks everyone to accept it again at their next message (NFR-PRIV-01).

The daily copy ([§16.2](#162-copy-off-the-server)) picks up the changed file.

## 15. Deploy and roll back

Deploying means replacing the application and restarting (NFR-MNT-01). The schema updates itself at startup, so never change it by hand.

### 15.1 Deploy a new version

Pick a quiet time. The app is down for several seconds, and LINE resends what it missed if redelivery is on. LINE doesn't guarantee that, or say how soon it retries.

1. Note what's running. The part after the `:` is `<running-version>`, and it's what you would roll back to. `/health` shows the same number without the `v`.

   ```sh
   docker inspect --format '{{.Config.Image}}' iconmarket
   ```

2. See whether the new version needs new config keys. A missing required key stops the app from starting, so add any new ones to the config file now.

   ```sh
   old=<running-version> new=<version>
   diff <(curl -fsSL https://raw.githubusercontent.com/two-net/iconmarket/$old/src/IconMarket/config.sample.toml) \
        <(curl -fsSL https://raw.githubusercontent.com/two-net/iconmarket/$new/src/IconMarket/config.sample.toml)
   ```

3. Take a backup. You need it if the new version can't be rolled back.

   ```sh
   sudo -u postgres iconmarket-backup
   ```

4. Start the new version and check it. If the image can't be pulled, the script stops and the running app is untouched.

   ```sh
   iconmarket-start <version>
   docker logs iconmarket 2>&1 | grep -E 'database schema is up to date|accepting requests'
   curl -fsS https://<host>/health
   ```

### 15.2 Roll back

```sh
iconmarket-start <previous-version>
```

That is usually enough, because migrations only add (MIG-03) and an older version ignores what it doesn't know. A warning `column kept for a removed model property, now nullable` is expected: it means the older version doesn't know a column the newer one added, and the column stays.

If the older version exits with `the models would drop or rename`, the newer one added something the older one can't accept. It has changed nothing. Either go forward to a fixed version, or restore the backup from step 3 ([§16.4](#164-restore)) and start the older version, which loses everything since that backup.

### 15.3 Publish a new version

This is for whoever maintains the code. CI builds only on version tags.

1. Raise `version` in `src/IconMarket/Cargo.toml` and run `cargo build` so that `Cargo.lock` follows. `/health` reports this number, and it's how you tell versions apart.
2. Commit and push the branch, then tag: `git tag <version> && git push origin <version>`.

CI runs the tests, then publishes `ghcr.io/two-net/iconmarket:<version>`. The image is public, so the server needs no login.

## 16. Backup and restore

The database and the config file are everything needed to restore the service, because images are in the database (NFR-REL-07). The start script is worth keeping too, for the public page's four settings, so [§16.2](#162-copy-off-the-server) copies it as well.

| Target | Value | How it's met |
| --- | --- | --- |
| RPO, data you can lose | 24 hours | A dump every night. If the whole server is lost, add the time until that dump reached the second computer, so copy it soon after |
| RTO, time to be running again | 4 hours | [§16.4](#164-restore) on a new server. Time it before go-live |

Backups contain personal data and payment slips, and the config copy contains credentials. Keep both private, and never put them in git.

### 16.1 Nightly dump

Create `/usr/local/bin/iconmarket-backup`:

```sh
#!/bin/sh
# Dumps the database, checks the dump can be read, and keeps the newest 7.
# Run as the postgres user.
set -eu
umask 077
dir=/var/backups/iconmarket
tmp="$dir/in-progress.tmp"
trap 'rm -f "$tmp"' EXIT

pg_dump --format=custom --file="$tmp" iconmarket
pg_restore --list "$tmp" > /dev/null
mv "$tmp" "$dir/iconmarket-$(date +%Y%m%d-%H%M%S).dump"

cd "$dir"
ls -1t iconmarket-*.dump | tail -n +8 | while read -r old; do rm -- "$old"; done
```

Then install it and take the first backup:

```sh
chmod 755 /usr/local/bin/iconmarket-backup
install -d -m 700 -o postgres -g postgres /var/backups/iconmarket
echo '0 2 * * * postgres /usr/local/bin/iconmarket-backup' > /etc/cron.d/iconmarket-backup
sudo -u postgres iconmarket-backup
ls -l /var/backups/iconmarket
```

The time is by the server's clock, which is often UTC. Seven backups are kept so a bad one can be noticed in time, and no more because backups hold data that retention has since removed from the database (SRS §5.4). `pg_dump` is safe to run while the app is serving.

### 16.2 Copy off the server

A backup on the server doesn't survive losing the server. Do this once on your second computer:

```sh
mkdir -p ~/iconmarket-backups
chmod 700 ~/iconmarket-backups
ssh-keygen -t ed25519 -N '' -f ~/.ssh/iconmarket
```

Add the contents of `~/.ssh/iconmarket.pub` to `/root/.ssh/authorized_keys` on the server, creating the file if needed. The key has no passphrase so that the copy can run unattended; it gives root on the server, so protect this computer.

Save this as `~/iconmarket-backups/copy.sh`, with your `<host>`:

```sh
#!/bin/sh
# Copies the backups, the config file and the start script off the server,
# then keeps the newest 7 backups here.
set -eu
host=<host>
dir="$HOME/iconmarket-backups"
export RSYNC_RSH="ssh -i $HOME/.ssh/iconmarket"

rsync -a --exclude='*.tmp' "root@$host:/var/backups/iconmarket/" "$dir/"
rsync -a "root@$host:/etc/iconmarket/config.toml" "$dir/"
rsync -a "root@$host:/usr/local/bin/iconmarket-start" "$dir/"

cd "$dir"
ls -1t iconmarket-*.dump | tail -n +8 | while read -r old; do rm -- "$old"; done
```

Run it once by hand, answering `yes` to the host key question:

```sh
sh ~/iconmarket-backups/copy.sh
```

Then schedule it daily, about half an hour after the dump, allowing for any difference between the two computers' clocks. With `crontab -e` that is a line like:

```
30 2 * * * sh $HOME/iconmarket-backups/copy.sh
```

Don't use `rsync --delete`: a wiped server would then wipe your copies too. No paid storage service is needed (NFR-COST-03).

### 16.3 Check it

Cron failures are silent, so look once a week. On the second computer:

```sh
ls -lt ~/iconmarket-backups | head -n 4
```

The newest dump should be from today, or from last night if today's hasn't run yet. Repeat a test restore after major changes, not only before go-live.

### 16.4 Restore

**On a new server.** Use the same LINE OA: LINE user IDs belong to its provider, so a backup can't move to a different one.

1. Point `<host>` at the new server, then do [§4](#4-server), [§5](#5-postgresql) and [§6](#6-https). In §5, use the `database.password` from your saved config rather than a new password.
2. On the second computer, run `ssh-keygen -R <host>`, then add its key to the new server as in [§16.2](#162-copy-off-the-server). Copy your saved files up, using the newest dump. A large one over a home connection can take a good part of the 4 hours.

   ```sh
   cd ~/iconmarket-backups
   scp -i ~/.ssh/iconmarket config.toml iconmarket-start iconmarket-<date>.dump root@<host>:/root/
   ```

3. On the new server, put them in place. Also create `/usr/local/bin/iconmarket-wait` as in [§8](#8-start-script).

   ```sh
   cd /root
   mkdir -p /etc/iconmarket
   install -o 10001 -g 10001 -m 600 config.toml /etc/iconmarket/config.toml
   install -m 755 iconmarket-start /usr/local/bin/iconmarket-start
   ```

4. Restore the dump. If `pg_restore` stops with an error, drop and recreate the database as in the same-server steps below, then run it again.

   ```sh
   install -d -m 700 -o postgres -g postgres /var/backups/iconmarket
   install -m 600 -o postgres iconmarket-<date>.dump /var/backups/iconmarket/
   sudo -u postgres pg_restore --no-owner --role=iconmarket --exit-on-error --dbname=iconmarket /var/backups/iconmarket/iconmarket-<date>.dump
   ```

5. Start the version you were running, or a newer one. An older one may refuse the schema ([§15.2](#152-roll-back)).

   ```sh
   iconmarket-start <version>
   ```

6. Check `https://<host>/health`, click **Verify** in the console, send the OA a message, and browse a product with a photo. The webhook URL stays the same, because `app.baseUrl` does.
7. Set up [§16.1](#161-nightly-dump) again, then run `copy.sh` once by hand on the second computer.

**On the same server,** for example to undo a bad deploy. To undo one, use `<previous-version>` in the last line:

```sh
docker stop iconmarket
sudo -u postgres dropdb iconmarket
sudo -u postgres createdb --owner=iconmarket --encoding=UTF8 --template=template0 iconmarket
sudo -u postgres pg_restore --no-owner --role=iconmarket --exit-on-error --dbname=iconmarket /var/backups/iconmarket/iconmarket-<date>.dump
iconmarket-start <version>
```

### 16.5 What a restore loses

Everything since the dump: orders, shops, products and registrations. Tell the village what to re-enter.

**Accounts deleted since the dump come back,** which matters under the PDPA because those people asked to be removed. IconMarket has no operator command to delete an account: only the person can, with **ลบบัญชี**. So find each one, ask them to delete it again if you can reach them, and otherwise have an admin suspend it under **ผู้ใช้** so that it and its shop stay hidden.

```sh
journalctl CONTAINER_NAME=iconmarket --no-pager | grep '"account deleted"'
sudo -u postgres psql -d iconmarket -c 'select id, display_name from users where id in (<user_id>, <user_id>)'
```

Each log line has a `user_id`, and the query turns it into the name admins search for. After a restore on a new server these logs are gone, unless you can still reach the old server.

A rich menu can also stay out of step for a user whose role changed since the dump, because LINE keeps its own link. It corrects itself the next time their roles change, and `เมนู` always works.

## 17. Optional: village group digest

"ของใหม่วันนี้" is a daily post to the village's LINE group (FR-NOT-07). It costs one push per group member every day.

1. In the console's **Messaging API** tab, turn on **Allow bot to join group chats**.
2. Add the OA to the group.
3. An admin types `#groupid` in the group, and the bot replies with the ID.
4. Set `line.villageGroupId` to that ID and `notification.groupDigest = true`, then restart.

## 18. Troubleshooting

Start with `docker ps -a` and `docker logs --tail 50 iconmarket`. A container showing `Restarting` is exiting at startup, and the logs say why. Older logs are in `journalctl CONTAINER_NAME=iconmarket`.

| Symptom | Cause | Fix |
| --- | --- | --- |
| `invalid config file` followed by a list | Mistakes in the config file; all are listed at once | Fix each, then restart |
| `cannot read config file` | Uid 10001 can't read it, or it isn't mounted. Saving with some editors or tools can change the owner | `chown 10001:10001` and `chmod 600` the file, then restart |
| `cannot connect to the database` | PostgreSQL is down, or `database.*` is wrong | `pg_lsclusters` should show `online`. Check the password and that `database.host` is `localhost` |
| `the database encoding is ..., but Thai text needs UTF8` | Database created without UTF-8 | Drop it and recreate it as in [§5](#5-postgresql) |
| `the database user needs rights to create and alter objects in its schema` | The user doesn't own the database | `sudo -u postgres psql -c 'ALTER DATABASE iconmarket OWNER TO iconmarket'` |
| `the models would drop or rename` | The running version is older than the schema | See [§15.2](#152-roll-back) |
| `cannot open log file` | `logging.file` points somewhere uid 10001 can't write | Leave it empty |
| `rich-menu provisioning failed, continuing` | Wrong or reissued access token; the app runs without rich menus | Fix `line.channelAccessToken` and restart. `เมนู` still works |
| **Verify** fails with a 400 status, and the log says `webhook request refused: missing or invalid signature` | Wrong `line.channelSecret` | Copy it again from **Basic settings** and restart |
| **Verify** fails with a 502 status | The app is down | Check `docker ps` |
| **Verify** fails with a certificate or connection error | DNS, firewall or Caddy | Run the check in [§6](#6-https) |
| **Verify** works but the bot says nothing, and the log shows nothing | **Use webhook** is off | Turn it on |
| The bot says nothing, and the log shows `reply failed` with `LINE answered 401 Unauthorized` | The access token was reissued or is wrong | Update `line.channelAccessToken` and restart |
| A LINE greeting or auto-reply appears alongside the bot's | LINE's own replies are on | Repeat [§2.4](#24-turn-off-lines-own-replies) |
| `/health` returns 503 with `"database":"down"` | PostgreSQL stopped after the app started | `systemctl restart postgresql`; the app reconnects |
| Pushes stop arriving, and the log shows `push failed; given up` with `the monthly message quota is used up` | Quota spent. Replies still work; those pushes are not retried | Raise the plan in OA Manager, or turn off optional pushes. LINE says this error can also appear with quota left: see its [error reference](https://developers.line.biz/en/reference/messaging-api/) |
| An AI feature doesn't work and startup logged `AI feature turned off: the model lacks what it needs` | The provider or model lacks a needed capability | Check the list in the sample file, or change `ai.provider` |
| AI answers fail, and the log shows `AI call failed` with `outcome` and `detail` | Bad key, retired model or empty balance. Everything that doesn't need AI keeps working (NFR-REL-06) | Fix `ai.apiKey` or `ai.model`, or fund the account, then restart |
| The first restart after adding an admin shows `"admins_granted":0`, or there's no admin tab | The ID is mistyped or from another LINE provider, or the rich menus failed | Compare it with the `#myid` reply from this OA. Check for `rich-menu provisioning failed`. `เมนู` still works |
| `iconmarket-start` fails at `docker pull` with `manifest unknown` or `no matching manifest` | The version doesn't exist, or the server is ARM | Check the [tags](https://github.com/two-net/iconmarket/tags). The running app is untouched |
| Backups stopped, or the disk is full | Dumps failing, or old images and logs piling up | `df -h`, then run `sudo -u postgres iconmarket-backup` and read the error. To free space: `docker image prune -a` and `journalctl --vacuum-size=1G` |

## 19. Monitoring

The target is 99.0 % availability a month, not counting planned maintenance (NFR-REL-04). In a 30-day month that allows about 7 hours 12 minutes of unplanned downtime.

**Set it up once.** Create a free account with an external uptime monitor, such as UptimeRobot, StatusCake or HetrixTools. Free plans change, so check when you sign up that the current terms allow your use. No paid plan is needed (NFR-COST-03). Then add one monitor:

| Setting | Value |
| --- | --- |
| Type | HTTPS, with a keyword check if offered |
| URL | `https://<host>/health` |
| Keyword | `"status":"ok"` |
| Interval | 5 minutes |
| Alert | Your email, and a phone notification if the service has an app |

It has to run outside the server, because a check on the server can't report the server being gone. The second computer won't do either, as it may be off.

**Test the alert.** Do this before villagers join. On the server, stop the app, wait for the alert, then start it again and wait for the recovery notice:

```sh
docker stop iconmarket
docker start iconmarket
```

**What it can't see.** `/health` shows that the app answers through HTTPS and can reach PostgreSQL (NFR-MNT-04). It doesn't show that messages are being handled. So after a deploy, or when a villager says the bot has gone quiet, send the OA a message yourself.

**Each month.** Read the month's uptime percentage in the monitor's report. Leave out your deploys and other planned work, which the target excludes. If it's still under 99.0 %, find the cause in [§18](#18-troubleshooting) before it happens again. A 5-minute check can miss a shorter outage, which is fine for a budget of hours.

When an alert fires, start with [§18](#18-troubleshooting).
