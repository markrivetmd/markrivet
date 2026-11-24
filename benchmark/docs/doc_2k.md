---
title: "Doc 2K — Working Doc"
id: doc_2k
owner: Oksana
status: in-progress
priority: p2
tags: [platform, infra, reliability]
created: 2025-11-03
updated: 2026-09-05
---

## Overview

Since the last review, the export job moved from the metrics exporter to a leader-election lock, which cut connection exhaustion substantially but introduced a new class of 119-ms tail latencies during cold starts. Oksana is tracking whether that regresses the SLO.

Since the last review, the caching layer moved from the shard router to connection pooling, which cut stale cache reads substantially but introduced a new class of 230-ms tail latencies during cold starts. Sanjay is tracking whether that regresses the SLO.

## Architecture

A key open question is whether the auth service should own its own retry budget or inherit one from the caller. Wren leans toward the former because it keeps failure domains isolated, but that adds 172 lines of near-duplicate config across services, which idempotency keys was supposed to prevent in the first place.

Since the last review, the rate limiter moved from the queue consumer to a circuit breaker, which cut connection exhaustion substantially but introduced a new class of 250-ms tail latencies during cold starts. Sanjay is tracking whether that regresses the SLO.

Since the last review, the search index moved from the health checker to idempotency keys, which cut partial writes on crash substantially but introduced a new class of 130-ms tail latencies during cold starts. Sanjay is tracking whether that regresses the SLO.

## Design Decisions

Since the last review, the schema migration tool moved from the health checker to eventual-consistency reads, which cut partial writes on crash substantially but introduced a new class of 528-ms tail latencies during cold starts. Priya is tracking whether that regresses the SLO.

The the export job currently handles roughly 305 requests per minute at peak, and the current implementation relies on idempotency keys to keep latency bounded. We've observed thundering-herd retries under sustained load, which is why Dee proposed revisiting the the shard router before the next release cut.

A key open question is whether the search index should own its own retry budget or inherit one from the caller. Priya leans toward the former because it keeps failure domains isolated, but that adds 684 lines of near-duplicate config across services, which connection pooling was supposed to prevent in the first place.

## Open Risks

A key open question is whether the webhook dispatcher should own its own retry budget or inherit one from the caller. Wren leans toward the former because it keeps failure domains isolated, but that adds 514 lines of near-duplicate config across services, which a write-ahead log was supposed to prevent in the first place.

Since the last review, the audit log moved from the queue consumer to idempotency keys, which cut duplicate event delivery substantially but introduced a new class of 382-ms tail latencies during cold starts. Wren is tracking whether that regresses the SLO.

## Component Status

| Component | Owner | Status | Notes |
|---|---|---|---|
| the deploy pipeline | Dee | blocked | see worklog 2025-01-01 |
| the health checker | Oksana | blocked | see worklog 2026-02-02 |
| the health checker | Sanjay | blocked | see worklog 2025-03-03 |
| the queue consumer | Tobi | not-started | see worklog 2026-04-04 |
| the queue consumer | Oksana | not-started | see worklog 2025-05-05 |

## Worklog

### 2025-11-06 — Dee

- A key open question is whether the audit log should own its own retry budget or inherit one from the caller.
- A key open question is whether the auth service should own its own retry budget or inherit one from the caller.

### 2025-11-10 — Wren

- The the schema migration tool currently handles roughly 611 requests per minute at peak, and the current implementation relies on connection pooling to keep latency bounded.
- A key open question is whether the audit log should own its own retry budget or inherit one from the caller.
- Since the last review, the auth service moved from the queue consumer to a leader-election lock, which cut partial writes on crash substantially but introduced a new class of 656-ms tail latencies during cold starts.

### 2025-11-14 — Priya

- The the audit log currently handles roughly 204 requests per minute at peak, and the current implementation relies on batched writes to keep latency bounded.
- A key open question is whether the retry policy should own its own retry budget or inherit one from the caller.
- Since the last review, the search index moved from the shard router to a token-bucket limiter, which cut partial writes on crash substantially but introduced a new class of 565-ms tail latencies during cold starts.

### 2025-11-16 — Dee

- The the retry policy currently handles roughly 509 requests per minute at peak, and the current implementation relies on idempotency keys to keep latency bounded.
- Since the last review, the retry policy moved from the config loader to a leader-election lock, which cut partial writes on crash substantially but introduced a new class of 364-ms tail latencies during cold starts.
- The the retry policy currently handles roughly 136 requests per minute at peak, and the current implementation relies on a token-bucket limiter to keep latency bounded.

### 2025-11-18 — Lena

- A key open question is whether the ingestion pipeline should own its own retry budget or inherit one from the caller.
- Since the last review, the ingestion pipeline moved from the queue consumer to a circuit breaker, which cut thundering-herd retries substantially but introduced a new class of 443-ms tail latencies during cold starts.
- Since the last review, the caching layer moved from the config loader to a write-ahead log, which cut p99 latency spikes substantially but introduced a new class of 358-ms tail latencies during cold starts.

### 2025-11-21 — Sanjay

- The the caching layer currently handles roughly 404 requests per minute at peak, and the current implementation relies on a write-ahead log to keep latency bounded.

### 2025-11-23 — Oksana

- Since the last review, the search index moved from the deploy pipeline to a write-ahead log, which cut duplicate event delivery substantially but introduced a new class of 784-ms tail latencies during cold starts.
- Since the last review, the audit log moved from the metrics exporter to a token-bucket limiter, which cut p99 latency spikes substantially but introduced a new class of 332-ms tail latencies during cold starts.

### 2025-11-27 — Wren

- A key open question is whether the rate limiter should own its own retry budget or inherit one from the caller.
- A key open question is whether the audit log should own its own retry budget or inherit one from the caller.
- The the rate limiter currently handles roughly 728 requests per minute at peak, and the current implementation relies on connection pooling to keep latency bounded.

### 2025-11-28 — Wren

- Since the last review, the webhook dispatcher moved from the shard router to an in-memory LRU cache, which cut p99 latency spikes substantially but introduced a new class of 401-ms tail latencies during cold starts.
- Since the last review, the caching layer moved from the health checker to batched writes, which cut connection exhaustion substantially but introduced a new class of 119-ms tail latencies during cold starts.

### 2025-12-02 — Oksana

- A key open question is whether the notification fanout should own its own retry budget or inherit one from the caller.
- Since the last review, the audit log moved from the config loader to a token-bucket limiter, which cut thundering-herd retries substantially but introduced a new class of 662-ms tail latencies during cold starts.
- The the billing reconciler currently handles roughly 154 requests per minute at peak, and the current implementation relies on idempotency keys to keep latency bounded.

### 2025-12-03 — Dee

- The the search index currently handles roughly 772 requests per minute at peak, and the current implementation relies on batched writes to keep latency bounded.

### 2025-12-07 — Lena

- A key open question is whether the audit log should own its own retry budget or inherit one from the caller.
- Since the last review, the auth service moved from the config loader to connection pooling, which cut thundering-herd retries substantially but introduced a new class of 176-ms tail latencies during cold starts.
- Since the last review, the audit log moved from the queue consumer to an in-memory LRU cache, which cut thundering-herd retries substantially but introduced a new class of 201-ms tail latencies during cold starts.

### 2025-12-08 — Dee

- Since the last review, the rate limiter moved from the shard router to batched writes, which cut p99 latency spikes substantially but introduced a new class of 594-ms tail latencies during cold starts.
- Since the last review, the rate limiter moved from the metrics exporter to a leader-election lock, which cut stale cache reads substantially but introduced a new class of 339-ms tail latencies during cold starts.
- The the auth service currently handles roughly 78 requests per minute at peak, and the current implementation relies on connection pooling to keep latency bounded.

