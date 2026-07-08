# adb-proxy Phase 1 Release Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a first-stage Rust/Tokio adb transparent TCP proxy with GitHub Actions release artifacts for Linux static musl and macOS.

**Architecture:** The binary parses a small CLI, initializes tracing, then calls a library `run_proxy` loop. Each accepted client opens one upstream TCP connection, splits both sockets, copies both directions concurrently, and records byte counts plus duration in a session summary.

**Tech Stack:** Rust 2021, Tokio, tracing, clap, thiserror, GitHub Actions, cargo-zigbuild for Linux musl release artifacts.

---

### Task 1: Project Scaffold and Failing Proxy Test

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `tests/proxy.rs`

- [ ] **Step 1: Create minimal Rust scaffold and a failing integration test**

Create `Cargo.toml` with package metadata and dependencies. Create empty `src/lib.rs`. Create `tests/proxy.rs` with an async test that expects `adb_proxy::ProxyConfig`, `adb_proxy::run_proxy_with_shutdown`, and `adb_proxy::wait_for_port` to exist. The test starts a mock upstream echo server, starts the proxy on an ephemeral port, sends bytes through the proxy, and expects the same bytes back.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test proxy forwards_bytes_bidirectionally`

Expected: FAIL because exported API does not exist yet.

### Task 2: Minimal Transparent Proxy Library

**Files:**
- Modify: `src/lib.rs`

- [ ] **Step 1: Implement the minimal library API**

Implement `ProxyConfig`, `ProxyStats`, `ProxyError`, `run_proxy`, `run_proxy_with_shutdown`, `wait_for_port`, and internal connection handling. Use Tokio `TcpListener`, `TcpStream`, `copy_bidirectional`, `select!`, and `Instant`.

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test --test proxy forwards_bytes_bidirectionally`

Expected: PASS.

### Task 3: CLI Binary and README Usage

**Files:**
- Create: `src/main.rs`
- Modify: `README.md`

- [ ] **Step 1: Add CLI binary**

Add clap-based arguments: `--listen`, `--target`, and `--log-level`. Default to `0.0.0.0:5038`, `127.0.0.1:5037`, and `info`.

- [ ] **Step 2: Update README with build and usage**

Document local run, client alias, and release artifact expectations.

- [ ] **Step 3: Run build**

Run: `cargo build`

Expected: PASS.

### Task 4: Release Automation

**Files:**
- Create: `.github/workflows/release.yml`

- [ ] **Step 1: Add GitHub Actions release workflow**

Build targets: `x86_64-unknown-linux-musl`, `aarch64-apple-darwin`, `x86_64-apple-darwin`. Package tar.gz artifacts with README usage snippet. On tags `v*`, create a GitHub Release and upload packages.

- [ ] **Step 2: Validate workflow syntax by inspection and full project checks**

Run: `cargo test` and `cargo build --release`

Expected: PASS.

### Task 5: Local Git Commit

**Files:**
- Modify staged project files only.

- [ ] **Step 1: Check status**

Run: `git status -sb`

- [ ] **Step 2: Commit implementation**

Run: `git add README.md Cargo.toml src tests .github docs && git commit -m "feat: add adb proxy phase1"`

- [ ] **Step 3: Report GitHub publishing blocker if no remote or gh exists**

Run: `git remote -v` and `command -v gh`

Expected: local commit exists; remote publishing waits for remote and GitHub CLI availability.
