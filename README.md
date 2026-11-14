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
Требования: Git for Windows, Node.js `^20.19.0` или `>=22.12.0` (требование Vite 8; рекомендуется актуальный LTS), Rust stable **1.90+** (`x86_64-pc-windows-msvc`), Microsoft C++ Build Tools, WebView2 Runtime — см. официальные Tauri 2 prerequisites.

```powershell
git clone https://github.com/Inlerno/ATLAS.git C:\dev\atlas
cd C:\dev\atlas
npm ci
npm run tauri dev
```

В обычной сборке модулей пока нет: окно показывает панель модулей с текстом «Модули не зарегистрированы».

### Демо-модули
Для ручной проверки менеджера модулей есть три демо-модуля — `demo.base` ← `demo.service` ← `demo.client`. Они собираются только с feature `demo-modules`; в обычной сборке и в установщике их нет.

```powershell
npm run tauri dev -- --features demo-modules
```

Все три изначально отключены. Настройки (кнопка «Настройки», JSON) вызывают сбои по требованию:
- `fail_on_start: true` — модуль не запускается;
- `panic_after_secs: N` — фоновая задача модуля аварийно завершается через N секунд после запуска (`null` — никогда);
- `tick_interval_secs: N` (только `demo.service`) — период события `Tick`; `demo.client` пишет каждое полученное событие в журнал.

Настройки и решения «включён / отключён» пока хранятся только в памяти и теряются при закрытии приложения (постоянное хранение — отдельный этап).

### Проверки
```powershell
npm run build
cd src-tauri
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo clippy --all-targets --features demo-modules -- -D warnings
cargo test --features demo-modules
```

Держите репозиторий в коротком ASCII-пути вне OneDrive.

## Документация
- `CLAUDE.md` — проектные инструкции Claude.
- `docs/ARCHITECTURE.md` — архитектура.
- `docs/ROADMAP.md` — этапы.
- `docs/DEVELOPMENT_WORKFLOW.md` — процесс разработки с Claude (прямой доступ к репозиторию, PR, проверка на Windows).
- `docs/FIRST_STAGE.md` — первый этап.
- `docs/CLAUDE_CHAT_PROMPTS.md` — готовые запросы.
- `docs/DEVELOPMENT_LOG.md` — версии окружения и журнал проверок.
