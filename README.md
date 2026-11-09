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

### Cloudflare Access Authentication

To enable Cloudflare Access authentication, set the `CF_ACCESS_TEAM` and `CF_ACCESS_AUD` environment variables. When enabled, the application will validate the `Cf-Access-Jwt-Assertion` header on incoming requests.

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
- `/static/` - Static file serving

## License

[MIT](LICENSE)
