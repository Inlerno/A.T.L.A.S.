# ATLAS — Local AI Assistant

Модульный локальный голосовой ИИ-помощник для Windows 10/11. Проект создаётся с нуля; `Inlerno/Voice_Assistant` — только функциональный референс, не источник кода для автоматического копирования.

## Цель
Приложение с локальной LLM через LM Studio, голосовым вводом/выводом, гибридным интерфейсом Orb + Chat, управляемой памятью, Telegram-интеграцией и безопасными командами управления Windows.

## Стек
- Tauri 2 + Rust + Tokio
- React + TypeScript + Vite
- Tailwind CSS + shadcn/ui, Framer Motion, Lucide
- Zustand
- SQLite + SQLx
- LM Studio как основной LLM provider через OpenAI-compatible API

## Принципы
1. Local-first; сетевые функции только как явно включённые интеграции.
2. Модульность и управляемый lifecycle.
3. Пользователь выбирает модели, движки и функции.
4. Никакого произвольного shell/PowerShell от LLM.
5. Сбой необязательного модуля не завершает приложение.
6. Python не является обязательным runtime конечного продукта.

## Быстрый старт (Windows)
Требования: Git for Windows, Node.js LTS, Rust stable (`x86_64-pc-windows-msvc`), Microsoft C++ Build Tools, WebView2 Runtime — см. официальные Tauri 2 prerequisites.

```powershell
git clone https://github.com/Inlerno/A.T.L.A.S..git C:\dev\atlas   # явное имя папки: Windows отбрасывает точку в конце
cd C:\dev\atlas
npm ci
npm run tauri dev
```

Проверки:
```powershell
npm run build
cd src-tauri
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Держите репозиторий в коротком ASCII-пути вне OneDrive.

## Документация
- `CLAUDE.md` — проектные инструкции Claude.
- `docs/ARCHITECTURE.md` — архитектура.
- `docs/ROADMAP.md` — этапы.
- `docs/DEVELOPMENT_WORKFLOW.md` — процесс Claude Chat + GitHub.
- `docs/FIRST_STAGE.md` — первый этап.
- `docs/CLAUDE_CHAT_PROMPTS.md` — готовые запросы.
- `docs/DEVELOPMENT_LOG.md` — версии окружения и журнал проверок.
