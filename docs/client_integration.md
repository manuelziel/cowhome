# Client Integration Guide

This guide explains how external clients (like the CowHome Android App or web dashboards) securely connect to the CowHome API.

## Design Philosophy

1. **Direct Communication:** The client connects directly to the CowHome instance (e.g., `https://home.libracow.com`).
2. **No Central Identity:** CowHome operates completely autonomously without external SSO dependencies.
3. **Stateless Auth:** Authentication uses a static API Key ($\ge 16$ printable ASCII characters).

## Authentication (`RFC 6750` Header-Only)

Every HTTP request to `/api/v1/*` (except the zero-secret `GET /api/v1/docs` HTML shell) must include the configured API key in an HTTP header:
- `Authorization: Bearer <KEY>`
- `Authorization: ApiKey <KEY>`
- `X-API-Key: <KEY>`

Passing `?api_key=` in the URL query string is rejected (`HTTP 401 Unauthorized` with `WWW-Authenticate: Bearer realm="CowHome"`).

## Rate Limiting & Trusted Proxies

1. **Global IP Flooding Limit (`ip:{client_ip}`):** Up to `120` requests/minute per IP (`HTTP 429 Too Many Requests`).
2. **Authentication Brute-Force Lockout (`login:{client_ip}`):** Up to `5` failed API key attempts within `window = 3600` seconds; reset immediately on valid authentication.
3. **Trusted Proxies:** `X-Forwarded-For` / `X-Real-IP` headers are evaluated only when the immediate TCP peer matches `[api] trusted_proxies` (default `["127.0.0.1", "::1"]`).

## Multi-Device Offline-Sync (Last-Write-Wins, Command TTL & Clock Skew)

1. **Client Timestamps:** Write endpoints accept `"timestamp"` / `"client_timestamp"` in JSON or `X-Client-Timestamp` header (auto-normalizing Unix seconds and millisecond epochs).
2. **Clock-Skew Clamping:** Server clamps incoming timestamps to `min(client_timestamp, server_now)`.
3. **LWW & Command TTL (`300 s`):** Stale writes (`client_timestamp < last_applied_timestamp`) or expired direct hardware actions return `HTTP 200 OK` with `"ignored_stale": true` in `data`.
4. **Two-Pillar Time Model:** Point-in-time events use UTC Unix epochs; recurring `"HH:MM"` weekly schedules are evaluated in the home IANA timezone (`[weather] timezone`, default `"Europe/Berlin"`).
