# Architecture Decision Records

- **ADR-001 Desktop:** Tauri 2; business logic не размещать в Tauri command handlers.
- **ADR-002 Core:** Rust; Python не является runtime-зависимостью конечного приложения.
- **ADR-003 Frontend:** React + TypeScript + Vite; Tailwind, shadcn/ui, Framer Motion, Lucide; Zustand.
- **ADR-004 LLM:** LM Studio — основной provider через OpenAI-compatible API за адаптером.
- **ADR-005 Storage:** SQLite + SQLx + migrations; сначала relational queries/FTS5, vector retrieval позже.
- **ADR-006 UX:** Orb + полноценный чат; advanced settings; пользователь выбирает модули.
- **ADR-007 Voice:** адаптеры STT/TTS/VAD/wake-word.
- **ADR-008 PC control:** типизированные allowlisted команды; произвольное выполнение shell/PowerShell моделью запрещено.
- **ADR-009 Workflow:** ~~Claude Chat + GitHub connector; пользователь применяет патчи сам.~~ Заменено ADR-010.
- **ADR-010 Workflow (2026-11-08):** Claude работает с прямым доступом на запись: создаёт issue, ветки, коммиты и PR и выполняет проверки в облачном Linux. Прямые коммиты в `main` запрещены; PR сливает только пользователь после ручной проверки на Windows. *Последствия:* облачные проверки подтверждают компиляцию и тесты, но не поведение на Windows; Windows-проверка остаётся обязательным шагом перед merge.
