# Development workflow — Claude с прямым доступом к репозиторию

См. ADR-010 в `docs/DECISIONS.md`.

## Роли
| Кто | Что делает |
|---|---|
| Claude | Анализ, план, issue, ветка, код, тесты, проверки в облачном Linux, PR с описанием проверок |
| Пользователь | Согласует план, проверяет на Windows, делает ревью, **сливает PR** |

Облачное окружение Claude — Linux. Сборка, `cargo test`, `clippy`, `npm run build` там подтверждают компиляцию и логику, но не поведение на Windows (WebView2, MSVC, пути, микрофон, Credential Manager и т. п.).

## Цикл задачи
1. Пользователь формулирует задачу.
2. Claude изучает репозиторий и документацию, предлагает план: архитектура, файлы, этапы, acceptance criteria, риски. **Код не пишется до согласования.**
3. Claude создаёт issue с согласованным планом.
4. Claude создаёт ветку от актуального `main` (или от ветки незаслитого зависимого PR — с пометкой в PR).
5. Claude реализует, запускает проверки, коммитит и открывает PR с `Closes #N`, описанием проверок и списком того, что нужно проверить на Windows.
6. Пользователь проверяет на Windows:
   ```powershell
   cd C:\dev\atlas
   git fetch origin
   git switch <branch>
   npm ci
   npm run build
   npm run tauri dev
   cd src-tauri
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
7. При ошибках пользователь присылает команду и полный вывод; Claude исправляет в той же ветке.
8. Пользователь сливает PR. Claude перед следующей задачей синхронизируется с `main`.

## Ветки
- `main`: собираемая база; прямые коммиты запрещены.
- `<type>/<issue>-short-name`, где type ∈ `feature`, `fix`, `docs`, `chore`.
- Один issue — одна ветка — один PR.

## Ограничения Claude
- Не сливает PR, не делает force push в общие ветки, не меняет настройки репозитория, не удаляет ветки без просьбы.
- Не утверждает, что что-то работает на Windows, без подтверждения пользователя.

Human review is mandatory for dependencies/licenses, migrations, OS-control features, permissions/secrets, installer scripts and destructive operations.
