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

## Module Manager
Решения — ADR-011 (core-сервисы и compile-time модули) и ADR-012 (двухфазные изменения). Код — `src-tauri/src/modules/`.

- **ID модуля:** `domain.name` — строчные латинские буквы, цифры и `_`, не меньше двух непустых сегментов (`ai.lmstudio`, `voice.stt`).
- **Граф зависимостей** (`graph.rs`) проверяется при сборке каталога: повторяющиеся ID, неизвестные зависимости, зависимость от самого себя, циклы (ошибка содержит путь цикла). Порядок запуска — топологический, при равенстве по ID; порядок остановки — строго обратный.
- **Статус** = `enabled` (решение пользователя) + `state` (факт):

```text
Stopped ─enable─▶ Initializing ─▶ Starting ─▶ Running ─disable/restart─▶ Stopping ─▶ Stopped
                       └──── ошибка / паника / таймаут ────────────────────────▶ Failed{phase}
Running ── зависимость упала ──▶ Blocked{by, DependencyFailed}          (enabled остаётся true)
включён, но зависимость выключена ──▶ Blocked{by, DependencyDisabled}
Failed / Blocked ── только подтверждённый recover ──▶ …                  (автоматических переходов нет)
```

- **Планы** (`plan.rs`) строятся без побочных эффектов:

| Действие | Применимо к | Затронутые | Подтверждение |
|---|---|---|---|
| `enable` | `enabled = false` | выключенные транзитивные зависимости (будут включены и запущены) | если затронутые есть |
| `disable` | `enabled = true` | включённые транзитивно зависимые (будут отключены; остановка в обратном порядке) | если затронутые есть |
| `recover` | `Failed`, `Blocked` | недостающие зависимости цели и заблокированные зависимые, которых план восстанавливает | **всегда** |
| `restart` | `Running` | работающие транзитивно зависимые (остановка в обратном порядке, затем запуск) | если затронутые есть |

Зависимость в `Failed`, `Blocked` или в переходном состоянии делает план неприменимым (`blockers`). `apply` (#9) выполняет только план, одобренный `verify`; результаты: `Approved`, `ConfirmationRequired`, `ImpactChanged`, `NotApplicable`.
- Изменения статусов публикуются только через центральную шину событий (#8).

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
