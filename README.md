# Local AI Assistant

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

## Документация
- `CLAUDE.md` — проектные инструкции Claude.
- `docs/ARCHITECTURE.md` — архитектура.
- `docs/ROADMAP.md` — этапы.
- `docs/DEVELOPMENT_WORKFLOW.md` — процесс разработки с Claude (прямой доступ к репозиторию, PR, проверка на Windows).
- `docs/FIRST_STAGE.md` — первый этап.
- `docs/CLAUDE_CHAT_PROMPTS.md` — готовые запросы.
