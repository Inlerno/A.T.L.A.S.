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
5. Claude реализует, запускает проверки, коммитит и открывает PR с `Closes #N`, описанием проверок и списком того, что нужно проверить на Windows. На каждый PR и push в `main` автоматически запускается CI на `windows-latest` (`.github/workflows/ci.yml`: `npm ci`, `npm run build`, `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`); результат и ссылка на run — в PR. Логи Actions Claude недоступны (видны только итог и аннотации), поэтому при падении CI пользователь при необходимости пересылает лог упавшего шага.
6. Для PR с кодом или конфигурацией сборки пользователь проверяет на Windows (для PR только с документацией достаточно ревью; отложенная проверка отмечается в PR и `docs/DEVELOPMENT_LOG.md`):
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
8. Пользователь сливает PR (условия — в `CLAUDE.md`, раздел «Git и GitHub»). Claude перед следующей задачей синхронизируется с `main`.

## Ветки
- `main`: собираемая база; прямые коммиты запрещены.
- `<type>/<issue>-short-name`, где type ∈ `feature`, `fix`, `docs`, `chore`.
- Один issue — одна ветка — один PR.

## Ограничения Claude
- Не сливает и не одобряет PR, не делает force push в опубликованные ветки, не меняет настройки репозитория, не удаляет ветки без просьбы.
- Не утверждает, что что-то работает на Windows, без подтверждения пользователя.

Ручное ревью пользователя обязательно для: зависимостей и лицензий, migrations, OS-control, permissions/secrets, installer scripts и деструктивных операций.
