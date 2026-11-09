# Screen Time Monitor

A real-time web dashboard for monitoring screen time usage across multiple users and hosts. This application consumes screen time events from a NATS JetStream server and provides a live view of usage statistics via a WebSocket-enabled web interface.

## Features

- **Real-time Monitoring**: Updates instantly as new screen time events are received via WebSockets.
- **NATS JetStream Integration**: Reliable event consumption using NATS JetStream.
- **Cloudflare Access Support**: Optional middleware for secure authentication behind Cloudflare Access.
- **Responsive Dashboard**: View detailed statistics including:
  - Time remaining today
  - Time spent today, this week, and this month
  - Current balance
  - Active host and user information

## Usage

### Prerequisites

- Rust toolchain (latest stable)
- NATS Server with JetStream enabled

### Running the Application

You can run the application using `cargo`:

```bash
cargo run --release
```

By default, the server listens on `127.0.0.1:3000`.

### Command Line Options

```bash
cargo run -- --help
```

- `-b, --bind <BIND>`: Address to bind the server to (default: "127.0.0.1:3000")
- `-u, --nats-url <NATS_URL>`: NATS server URL (default: "nats://localhost:4222")

## Configuration

The application can be configured via command-line arguments or environment variables.

| Environment Variable | Description | Default |
|---------------------|-------------|---------|
| `NATS_URL` | NATS server URL | `nats://localhost:4222` |
| `CF_ACCESS_TEAM` | Cloudflare Access team name | (Optional) |
| `CF_ACCESS_AUD` | Cloudflare Access audience tag | (Optional) |
| `DEV_IDENTITY` | Debug builds only: see [Local development](#local-development) | (Optional) |

### Cloudflare Access Authentication

To enable Cloudflare Access authentication, set the `CF_ACCESS_TEAM` and `CF_ACCESS_AUD` environment variables. When enabled, the application will validate the `Cf-Access-Jwt-Assertion` header on incoming requests.

A request with a valid JWT is an **adjuster**: the Cloudflare Access policy is the allowlist. A request without the header is let through as an anonymous viewer, so viewing directly over the LAN keeps working.

### Local development

Debug builds accept `--dev-identity <email>` (env `DEV_IDENTITY`), which attaches that identity to every request so you can act as an adjuster without Cloudflare, e.g. `cargo run -- --dev-identity me@example.com`. Release builds don't have the flag, and the web refuses to start when it is combined with `CF_ACCESS_TEAM`/`CF_ACCESS_AUD`.

## Data Format

The application reads the `OBSERVATIONS` JetStream stream (created by the collector; the web waits for it, retrying every 5 s) and expects messages on the subject `time.obs.<host>.<user>` with a JSON payload containing:

```json
{
  "left_day": 3600,
  "spent_balance": 1200,
  "spent_month": 50000,
  "spent_week": 12000,
  "spent_day": 4000
}
```

Values are in seconds.

## API Endpoints

- `/` - Main web interface
- `/ws` - WebSocket endpoint for real-time updates
- `/api/screentime` - REST API to fetch current screen time data
- `/api/me` - The viewer's identity: `{"email": "...", "adjuster": true}`, or `{"email": null, "adjuster": false}` when anonymous
- `POST /api/time-adjustments` - Time adjustment by an adjuster: `{"host", "user", "delta_seconds"}` (non-zero, within ±28800). Sent to the collector as a NATS request on `time.adjust.<host>.<user>`, waiting up to 15 s. Answers 200 with the collector's reply (`status: ok` / `error`), `unreachable` (no collector listening, no change made) or `timeout` (the change may or may not have happened), always with the `request_id` web made. Refusals are text: 401 without a verified identity, 400 for a bad body, 404 for a `host:user` web has no observations for, 415 unless the body is `application/json`
- `/static/` - Static file serving

## License

[MIT](LICENSE)
