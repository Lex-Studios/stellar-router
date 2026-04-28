# API Server Integration Guide

This document describes the new `router-api-server` component that provides transaction simulation and real-time status tracking for the stellar-router suite.

## Overview

The API server is an off-chain component that:
1. Exposes a `/simulate` endpoint for transaction preview (Issue #355)
2. Provides WebSocket support for real-time transaction status tracking (Issue #357)
3. Integrates with the `router-execution` contract for fee estimation and simulation

## Architecture

```
┌─────────────────────────────────────────────────────┐
│              router-api-server                      │
│  ┌──────────────────────────────────────────────┐   │
│  │  HTTP Handlers                               │   │
│  │  - GET  /health                              │   │
│  │  - POST /simulate                            │   │
│  │  - GET  /ws (WebSocket upgrade)              │   │
│  └──────────────────────────────────────────────┘   │
│  ┌──────────────────────────────────────────────┐   │
│  │  WebSocket Manager                           │   │
│  │  - Subscribe/Unsubscribe to tx_id            │   │
│  │  - Broadcast status updates                  │   │
│  │  - Handle reconnections                      │   │
│  └──────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────┘
         │                              │
         ▼                              ▼
    Soroban RPC              router-execution contract
```

## Issue #355: Transaction Simulation Endpoint

### Endpoint: `POST /simulate`

Allows developers to preview transaction outcomes without execution.

#### Request Format

```json
{
  "target": "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
  "function": "transfer",
  "route_details": {
    "name": "swap_route",
    "version": 1,
    "expected_outputs": ["1000000"]
  }
}
```

**Fields:**
- `target` (required): Target contract address
- `function` (required): Function name to invoke
- `route_details` (optional): Route breakdown information
  - `name`: Route identifier
  - `version`: Route version
  - `expected_outputs`: Expected output amounts

#### Response Format

```json
{
  "success": true,
  "estimated_fees": {
    "base_fee": 100,
    "resource_fee": 1000,
    "total_fee": 1100,
    "surge_multiplier": 100,
    "high_load": false
  },
  "expected_outputs": ["1000000"],
  "route_breakdown": {
    "route_name": "swap_route",
    "version": 1,
    "target_contract": "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
    "function": "transfer"
  },
  "message": "Simulation successful"
}
```

**Response Fields:**
- `success`: Whether simulation succeeded
- `estimated_fees`: Fee breakdown
  - `base_fee`: Network base fee in stroops
  - `resource_fee`: Resource (CPU/memory) fee in stroops
  - `total_fee`: Total estimated fee
  - `surge_multiplier`: Surge pricing multiplier (100 = 1x, 200 = 2x)
  - `high_load`: Whether high-load conditions detected
- `expected_outputs`: Array of expected output amounts
- `route_breakdown`: Route execution details
- `message`: Human-readable status message

#### Error Handling

- `400 Bad Request`: Missing or invalid parameters
- `500 Internal Server Error`: RPC or contract call failure

#### Example Usage

```bash
curl -X POST http://localhost:8080/simulate \
  -H "Content-Type: application/json" \
  -d '{
    "target": "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4",
    "function": "transfer",
    "route_details": {
      "name": "swap",
      "version": 1,
      "expected_outputs": ["1000000"]
    }
  }'
```

## Issue #357: Transaction Status Tracking via WebSocket

### Endpoint: `GET /ws`

Real-time transaction status updates via WebSocket.

#### Connection

```javascript
const ws = new WebSocket('ws://localhost:8080/ws');

ws.onopen = () => {
  console.log('Connected to transaction tracker');
};

ws.onmessage = (event) => {
  const message = JSON.parse(event.data);
  console.log('Received:', message);
};

ws.onerror = (error) => {
  console.error('WebSocket error:', error);
};

ws.onclose = () => {
  console.log('Disconnected from transaction tracker');
};
```

#### Subscribe to Transaction

Send a subscription message:

```json
{
  "action": "subscribe",
  "tx_id": "tx_12345"
}
```

Server response:

```json
{
  "msg_type": "subscribed",
  "data": {
    "tx_id": "tx_12345",
    "status": "subscribed"
  }
}
```

#### Status Events

The server broadcasts status updates:

```json
{
  "msg_type": "status_update",
  "data": {
    "tx_id": "tx_12345",
    "status": "PENDING",
    "timestamp": "2026-04-28T02:38:56Z",
    "message": "Transaction queued"
  }
}
```

**Supported Statuses:**
- `PENDING`: Transaction is pending
- `SUBMITTED`: Transaction submitted to network
- `CONFIRMED`: Transaction confirmed on-chain
- `FAILED`: Transaction failed

#### Unsubscribe from Transaction

```json
{
  "action": "unsubscribe",
  "tx_id": "tx_12345"
}
```

#### Reconnection Handling

Clients should implement automatic reconnection with exponential backoff:

```javascript
class TransactionTracker {
  constructor(url) {
    this.url = url;
    this.ws = null;
    this.subscriptions = new Set();
    this.retryDelay = 1000;
    this.maxRetryDelay = 30000;
  }

  connect() {
    this.ws = new WebSocket(this.url);
    
    this.ws.onopen = () => {
      console.log('Connected');
      this.retryDelay = 1000;
      // Re-subscribe to previous transactions
      this.subscriptions.forEach(txId => this.subscribe(txId));
    };

    this.ws.onmessage = (event) => {
      const message = JSON.parse(event.data);
      this.handleMessage(message);
    };

    this.ws.onclose = () => {
      console.log('Disconnected, retrying in', this.retryDelay, 'ms');
      setTimeout(() => this.connect(), this.retryDelay);
      this.retryDelay = Math.min(this.retryDelay * 2, this.maxRetryDelay);
    };
  }

  subscribe(txId) {
    this.subscriptions.add(txId);
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({
        action: 'subscribe',
        tx_id: txId
      }));
    }
  }

  unsubscribe(txId) {
    this.subscriptions.delete(txId);
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({
        action: 'unsubscribe',
        tx_id: txId
      }));
    }
  }

  handleMessage(message) {
    if (message.msg_type === 'status_update') {
      console.log(`Transaction ${message.data.tx_id}: ${message.data.status}`);
    }
  }
}

// Usage
const tracker = new TransactionTracker('ws://localhost:8080/ws');
tracker.connect();
tracker.subscribe('tx_12345');
```

## Deployment

### Prerequisites

- Rust 1.78+
- Soroban RPC endpoint
- Router execution contract ID

### Environment Variables

```bash
LISTEN_ADDR=127.0.0.1:8080
SOROBAN_RPC_URL=https://soroban-testnet.stellar.org
ROUTER_EXECUTION_CONTRACT_ID=CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4
```

### Local Development

```bash
cargo run -p router-api-server
```

### Docker

```bash
docker build -t router-api-server api-server/
docker run -p 8080:8080 \
  -e SOROBAN_RPC_URL=https://soroban-testnet.stellar.org \
  -e ROUTER_EXECUTION_CONTRACT_ID=CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4 \
  router-api-server
```

### Docker Compose

Add to `docker-compose.yml`:

```yaml
api-server:
  build:
    context: .
    dockerfile: api-server/Dockerfile
  ports:
    - "8080:8080"
  environment:
    LISTEN_ADDR: 0.0.0.0:8080
    SOROBAN_RPC_URL: https://soroban-testnet.stellar.org
    ROUTER_EXECUTION_CONTRACT_ID: ${ROUTER_EXECUTION_CONTRACT_ID}
  depends_on:
    - metrics
```

## Testing

### Unit Tests

```bash
cargo test -p router-api-server
```

### Integration Tests

```bash
# Start the server
cargo run -p router-api-server &

# Test simulation endpoint
curl -X POST http://localhost:8080/simulate \
  -H "Content-Type: application/json" \
  -d '{"target": "C...", "function": "transfer"}'

# Test WebSocket
wscat -c ws://localhost:8080/ws
```

## Performance Considerations

1. **Broadcast Channel**: Uses tokio broadcast for efficient multi-subscriber updates
2. **Connection Pooling**: Reuses HTTP connections to Soroban RPC
3. **Memory**: Tracks active subscriptions in DashMap for O(1) lookup
4. **Timeout**: WebSocket connections timeout after 5 minutes of inactivity

## Security

1. **Input Validation**: All inputs validated before processing
2. **Rate Limiting**: Can be added via middleware
3. **CORS**: Should be configured based on deployment needs
4. **TLS**: Use reverse proxy (nginx) for HTTPS in production

## Future Enhancements

1. **Contract Integration**: Direct calls to router-execution contract
2. **Transaction History**: Persistent storage of transaction status
3. **Metrics**: Prometheus metrics for monitoring
4. **Rate Limiting**: Per-IP or per-key rate limiting
5. **Authentication**: JWT or API key authentication
6. **Batch Simulation**: Simulate multiple transactions in one request
