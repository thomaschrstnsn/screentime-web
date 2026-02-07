# Screen Time Monitor

A web application built with Rust, Axum, and HTMX that displays real-time screen time data from NATS messages.

## Features

- **Real-time Updates**: WebSocket connection for live screen time data
- **NATS Integration**: Subscribes to `time.obs.<host>.<user>` subjects
- **Responsive UI**: Clean, modern interface using HTMX
- **User/Host Grouping**: Organizes data by user and host combinations
- **Activity Tracking**: Shows process names, window titles, and durations

## Requirements

- NATS server
- Rust 1.70+

## Configuration

The application can be configured using command-line arguments or environment variables:

### Command Line Arguments

```bash
# Start with default settings
cargo run

# Specify custom NATS URL and bind address
cargo run -- --nats-url nats://nats.example.com:4222 --bind 0.0.0.0:8080

# Specify custom subject pattern
cargo run -- --nats-subject "time.data.*"

# Show all options
cargo run -- --help
```

### Environment Variables

```bash
# Set NATS server URL
export NATS_URL="nats://nats.example.com:4222"

# Set NATS subject pattern
export NATS_SUBJECT="time.data.*"

# Then run with defaults
cargo run
```

### Configuration Priority

1. Command-line arguments (highest priority)
2. Environment variables
3. Default values (lowest priority)

**Default Values:**
- NATS URL: `nats://localhost:4222`
- Bind Address: `127.0.0.1:3000`
- Subject Pattern: `time.obs.*`

## NATS Message Format

The application expects JSON messages on subjects matching `time.obs.<host>.<user>` with the following structure:

```json
{
  "duration_seconds": 3600,
  "process_name": "chrome",
  "window_title": "GitHub - Developing awesome code"
}
```

## API Endpoints

- `/` - Main web interface
- `/ws` - WebSocket endpoint for real-time updates
- `/api/screentime` - REST API to fetch current screen time data
- `/static/` - Static file serving

## Architecture

- **NATS Subscriber**: Listens for screen time events
- **In-memory Storage**: Maintains recent activity data
- **WebSocket Broadcasting**: Real-time updates to connected clients
- **Responsive UI**: Clean dashboard with activity tracking