# 🚀 Open Claude Code

### AI Desktop + CLI Platform for Local & Cloud Models

---

## 🧠 Overview

**Open Claude Code** is a hybrid **AI desktop + CLI platform** designed to manage, route, and interact with both:

* 🖥️ **Local models** via Ollama
* ☁️ **Cloud models** via OpenRouter

It provides a unified, developer-friendly interface to control AI systems, execute tasks, and build AI-powered workflows.

---

## ✨ Core Capabilities

* 🔀 **Multi-Provider Support**
  Seamlessly switch between local (Ollama) and cloud (OpenRouter)

* 🧠 **Smart Routing System**

  * `LocalOnly` → Always local
  * `CloudOnly` → Always cloud
  * `SmartAuto` → Auto fallback logic

* 🤖 **Model Management**
  Select, configure, and switch models dynamically

* ⚡ **Connection Testing**
  Validate provider availability + latency insights

* 💾 **Persistent Settings**
  Unified settings stored securely via backend

* 🖥️ **Desktop Application (Tauri)**
  Native-like performance with Rust backend

* 💻 **CLI Interface (Bun Runtime)**
  Fast command-line interaction

---

## 🏗️ Tech Stack

### Frontend

* React 18
* TypeScript
* TailwindCSS
* Zustand (State Management)

### Backend

* Rust
* Tauri 2.x

### Runtime & Tooling

* Bun
* Vite
* Cargo

---

## 📁 Project Structure

```
open-claude-code-main/
│
├── src/                  # CLI application (TypeScript)
│   ├── entrypoints/
│   └── main.tsx
│
├── desktop/              # Desktop application (Tauri)
│   ├── src-ui/           # React UI
│   └── src-tauri/        # Rust backend
│       └── src/lib.rs
│
├── scripts/              # Build & utility scripts
├── dist/                 # CLI build output
```

---

## ⚙️ Requirements

### General

* Windows OS (primary supported environment)

### CLI

* Node.js ≥ 18
* Bun ≥ 1.1.0

### Desktop (Tauri)

* Rust (stable toolchain)
* WebView2 Runtime
* Visual Studio Build Tools (MSVC)
* Bun

---

## 🚀 Getting Started

---

### 🧩 CLI Setup

Install dependencies:

```bash
bun install
```

Run in development:

```bash
bun run dev
```

Type check:

```bash
bun run typecheck
```

Build:

```bash
bun run build
```

Run snapshot:

```bash
bun run snapshot -- --help
```

---

### 🖥️ Desktop Setup (Tauri)

#### 1. Install dependencies

```bash
cd desktop
bun install
```

#### 2. Run app

```bash
cd desktop/src-tauri
cargo tauri dev
```

#### 3. Build app

```bash
cargo tauri build
```

---

## 🎛️ AI Control Center

The desktop UI provides a centralized control system for AI configuration:

### 🔌 Providers

* Ollama (Local)
* OpenRouter (Cloud)

### 🔁 Routing Modes

* `LocalOnly`
* `CloudOnly`
* `SmartAuto` (recommended)

### ⚙️ Features

* Active model selection
* Parameter tuning (e.g. temperature)
* Connection testing
* Live status feedback

---

## 💾 Persistence System

All settings are managed via a **single source of truth**:

* Backend struct: `AppSettings`
* Stored as JSON on disk
* Unified save command:

```rust
save_all_settings(...)
```

---

## 🔐 Security Notes

* Never commit API keys
* Store secrets in backend only
* Browser mode falls back to `localStorage` (limited)

---

## 🔧 Configuration

### 🖥️ Ollama

Start service:

```bash
ollama serve
```

Pull a model:

```bash
ollama pull deepseek-coder:6.7b
```

Default URL:

```
http://localhost:11434
```

---

### ☁️ OpenRouter

Endpoint:

```
https://openrouter.ai/api/v1/chat/completions
```

Requires API key.

---

## 🧪 Troubleshooting

---

### ❌ ENOSPC Error

Free disk space:

* Delete:

```
desktop/src-tauri/target/
```

---

### ❌ Tauri API not working

Run via:

```bash
cargo tauri dev
```

NOT via browser only.

---

### ❌ Command not found

Ensure:

* `#[tauri::command]` is used
* Command is registered in:

```rust
tauri::generate_handler!(...)
```

* Restart build

---

## 🧠 Developer Notes

* Feature-flag heavy architecture
* Always validate with:

```bash
bun run typecheck
bun run build
cargo tauri dev
```

---

## 🔮 Future Direction

* AI agent orchestration
* Multi-step execution workflows
* File-aware AI editing
* Advanced routing & cost optimization
* Plugin system

---

## 📜 License

> Define your license here (MIT recommended if open-source)

---

## 💡 Final Note

This project is not just an interface —
it is a **foundation for building intelligent AI systems locally and in the cloud**.

---
