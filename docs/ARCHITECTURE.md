# Architecture baseline

```text
React UI (Chat / Orb / Settings / Diagnostics)
        │ typed Tauri IPC + events
        ▼
Rust Application Core
  ├─ Lifecycle / State / Event Bus
  ├─ Module Manager
  ├─ AI Engine
  ├─ Voice Engine
  ├─ Memory Engine
  ├─ Telegram Engine
  ├─ Windows/System Tools
  ├─ Settings / Diagnostics / Security
  └─ Storage (SQLite)
```

## Module lifecycle
Каждый optional module имеет stable ID, metadata, dependencies/capabilities, `initialize`, `start`, `stop`, `status`, `health_check`, структурированные ошибки. Отключение останавливает фоновые задачи и освобождает ресурсы. Циклы зависимостей запрещены.

## AI
LM Studio — default provider. Используй provider adapter. Поддержи connection test, model discovery где доступно, streaming, cancellation, generation settings и structured tool calls. Tool call — запрос, а не разрешение: отдельная policy layer валидирует действие.

## Voice
Отдельные интерфейсы/адаптеры для audio devices, STT, TTS, VAD и wake-word. Возможности и совместимость движков должны отражаться в настройках.

## Memory
История и долговременная память — разные сущности. Пользователь может просматривать, редактировать и удалять долговременную память. Начать с SQLite и FTS5; vector search отложить до доказанной необходимости.

## Windows tools
Только типизированные allowlisted операции. Read-only и изменяющие состояние возможности разделены. Чувствительные/необратимые действия требуют подтверждения. Не давать модели универсальный execute command.

## Secrets
Windows Credential Manager / DPAPI-backed storage или поддерживаемый эквивалент. Секреты не попадают в SQLite plaintext, логи, crash reports или экспорт диагностики.

## Target layout
`src/` для React; `src-tauri/src/` для Rust core и модулей; `src-tauri/migrations/`; `docs/`; `.github/`. Не создавать все каталоги заранее — добавлять структуру по мере появления реальных задач.
