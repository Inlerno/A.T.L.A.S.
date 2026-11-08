# Development log

Фиксирует точные версии окружения и команды, которыми каркас создавался и проверялся. Новые записи добавляются сверху.

---

## 2026-11-09 — Issue #6: ядро — модель ошибок, логирование, состояние приложения

### Решения (согласованы пользователем)
- **Fallback логирования.** Если файловый лог недоступен, приложение пишет только в stdout с предупреждением и продолжает работать. Сбой файлового логирования не останавливает приложение; `setup` никогда не возвращает ошибку (иначе Tauri паникует).
- **Ротация.** Новый файл раз в сутки, хранятся последние **7** файлов (`MAX_LOG_FILES`, включая текущий); старые удаляются автоматически.
  - Удаление выполняет `tracing-appender` 0.2.5: при создании appender'а (старт приложения) и при каждой смене дня. Удаляются только обычные файлы с префиксом `atlas` и суффиксом `log`, самые старые по времени создания.
  - Это не только заявлено: тест `rolling_appender_keeps_at_most_max_log_files` (10 старых файлов → ровно 7, посторонние файлы не тронуты) **падает**, если убрать `.max_log_files(...)` (проверено мутацией: `left: 11, right: 7`).
- **Путь к логам на Windows:** `%LOCALAPPDATA%\com.inlerno.atlas\logs\atlas.YYYY-MM-DD.log`. Дата в имени и момент ротации — по **UTC** (`tracing-appender`), для Калининграда это 02:00.
- **Сброс буфера при выходе.** `tauri::App::run` завершает процесс через `std::process::exit`, деструкторы управляемого состояния не вызываются. Поэтому guard логов хранится в `AppState` и явно сбрасывается в `flush_logs()` на `RunEvent::Exit`. `flush_logs()` идемпотентен, переживает отравленный мьютекс и одновременные вызовы.
- **Фильтр уровня:** `RUST_LOG`, иначе `info`. Значение передаётся в чистую функцию; тесты не меняют окружение процесса (`std::env::set_var` — `unsafe` в Rust 2024).
- Сообщения крейта `log` (в том числе от Tauri) перенаправляются в `tracing` (feature `tracing-log`).
- Panic hook пишет панику в лог (`target: panic`) и вызывает предыдущий hook — стандартное поведение сохраняется.

### Зависимости
Прямые (features объявлены явно):

| Крейт | Версия | Лицензия |
|---|---|---|
| `thiserror` (`std`) | 2.0.21 | MIT OR Apache-2.0 |
| `tracing` (`std`) | 0.1.44 | MIT |
| `tracing-subscriber` (`std`, `registry`, `fmt`, `ansi`, `env-filter`, `tracing-log`) | 0.3.23 | MIT |
| `tracing-appender` | 0.2.5 | MIT |

Новые транзитивные пакеты в `Cargo.lock` (только добавления, версии существующих не менялись): `lazy_static` 1.5.1, `matchers` 0.2.0, `nu-ansi-term` 0.50.3, `sharded-slab` 0.1.7, `symlink` 0.1.0, `thread_local` 1.1.10, `tracing-attributes` 0.1.31, `tracing-log` 0.2.0, `valuable` 0.1.1 — все MIT или MIT/Apache-2.0.

### Проверки в облачном Linux
| Проверка | Результат |
|---|---|
| `cargo fmt --check` | OK |
| `cargo clippy --locked --all-targets -- -D warnings` | OK, без `allow(dead_code)` |
| `cargo test --locked` | 14 тестов OK: 11 unit + 3 интеграционных бинарника по одному тесту |
| `npm run build` | OK |

Изоляция тестов: глобальный panic hook и глобальный подписчик проверяются в отдельных интеграционных бинарниках (`tests/panic_hook.rs`, `tests/logging_init_file.rs`, `tests/logging_init_fallback.rs`). Cargo запускает каждый отдельным процессом, в каждом ровно один тест, поэтому параллельные unit-тесты на них не влияют.

**Smoke-тест бинарника под Xvfb + openbox (Linux, НЕ Windows):** окно ATLAS появилось; закрытие через `wmctrl -c` (`WM_DELETE_WINDOW`, как кнопка закрытия) → код выхода 0. В файле лога все записи: `file logging enabled` → `ATLAS starting` → `exit requested` → `ATLAS exiting`; ANSI-последовательностей нет.
Наблюдение: окно из конфигурации создаётся раньше, чем выполняется `setup`, поэтому сообщения, появившиеся до `setup`, в файл не попадают.

### Ограничения
- При принудительном завершении процесса (`taskkill /F`, сбой нативного кода) последние буферизованные записи могут потеряться — `RunEvent::Exit` не наступает.
- Сообщения, появившиеся до `setup`, в файл не попадают.

### Ручная проверка на Windows — ОТЛОЖЕНА
> **Статус: ОТЛОЖЕНА** — решение пользователя от 2026-11-09 (нет доступа к Windows-машине; допускается `CLAUDE.md` / ADR-010).
> Сборка, `clippy` и тесты на Windows выполняет CI (`windows-latest`). Отложены:
> - [ ] `npm run tauri dev` — окно ATLAS открывается;
> - [ ] в `%LOCALAPPDATA%\com.inlerno.atlas\logs` создан `atlas.YYYY-MM-DD.log` со строками `file logging enabled` и `ATLAS starting`;
> - [ ] после закрытия окна в файле есть `exit requested` и `ATLAS exiting`, приложение закрывается без зависания;
> - [ ] `$env:RUST_LOG="debug"; npm run tauri dev` — в логе появляются записи уровня DEBUG.

---

## 2026-11-09 — Issue #5: Windows CI

Workflow: `.github/workflows/ci.yml`, job `windows` на `windows-latest`, `timeout-minutes: 45`, `permissions: contents: read`.

Триггеры:
- `push` в `main` и в `chore/5-windows-ci` (гарантирует первый запуск, даже если в базовой ветке PR ещё нет workflow);
- `pull_request`.

| Action | Версия | SHA | Лицензия |
|---|---|---|---|
| `actions/checkout` | v7.0.1 | `3d3c42e5aac5ba805825da76410c181273ba90b1` | MIT |
| `actions/setup-node` | v7.1.0 | `949feb2413d6458794dcd2491c4babbbce0c15c1` | MIT |
| `actions/cache` | v6.1.0 | `55cc8345863c7cc4c66a329aec7e433d2d1c52a9` | MIT |

Версии взяты из тегов репозиториев actions (`git ls-remote`); SHA проверены как коммиты с `action.yml` (`runs.using: node24`). Сторонние actions не используются; Rust ставится через предустановленный `rustup`.

Шаги: `npm ci` → `npm run build` → `cargo fmt --check` → `cargo clippy --locked --all-targets -- -D warnings` → `cargo test --locked`.

Проверка в облачном Linux: `actionlint` — OK.

### Результаты прогонов (GitHub Actions, `windows-latest`)
На каждый push в `chore/5-windows-ci` запускаются два прогона: по `push` и по `pull_request` (PR #14).

| Коммит | Что проверялось | Результат | Время | Прогоны |
|---|---|---|---|---|
| `cacf089` | первый запуск (репозиторий ещё `Inlerno/A.T.L.A.S.`) | ❌ job не стартовал: `Directory 'D:\a\A.T.L.A.S.\A.T.L.A.S.' does not exist` | 4 с / 6 с | [42530799004](https://github.com/Inlerno/ATLAS/actions/runs/42530799004/job/128884320728) + PR-прогон |
| `9d5f205` | после переименования в `Inlerno/ATLAS`; холодный кэш | ✅ success | 5:01 / 4:26 | [42532858253](https://github.com/Inlerno/ATLAS/actions/runs/42532858253/job/128890704475), [42532861682](https://github.com/Inlerno/ATLAS/actions/runs/42532861682/job/128890715471) |
| `e9f321c` | временное нарушение `rustfmt` | ❌ failure (ожидаемо), `exit code 1` | 0:49 / 0:50 | [42533321819](https://github.com/Inlerno/ATLAS/actions/runs/42533321819/job/128892096671), [42533326255](https://github.com/Inlerno/ATLAS/actions/runs/42533326255/job/128892110083) |
| `904c900` | revert нарушения; тёплый кэш | ✅ success | 1:36 / 1:43 | [42533441876](https://github.com/Inlerno/ATLAS/actions/runs/42533441876/job/128892450110), [42533447296](https://github.com/Inlerno/ATLAS/actions/runs/42533447296/job/128892465759) |

Выводы:
- Проверяемые коммиты содержат head PR #2 (`85f81e6`) целиком и меняют поверх него только файлы CI и документации (GitHub compare API), то есть **код PR #2 собран, проверен `clippy` и протестирован на Windows**.
- `cargo fmt --check` на Windows-раннере проходит: `.gitattributes` с `eol=lf` работает.
- Кэш ускоряет прогон примерно в 3 раза (≈5 мин → ≈1,5 мин).
- Причина первого сбоя — точка в конце имени репозитория. Репозиторий переименован пользователем в `Inlerno/ATLAS` (2026-11-09).

Ограничение: логи шагов Actions недоступны Claude (API Actions закрыт прокси сессии); видны только итог, время и аннотации. Шаг, на котором упал прогон `e9f321c`, определён по содержанию изменения и времени (до компиляции Rust дело не дошло), а не по логу.

---

## 2026-11-08 — Issue #1: Tauri 2 + React + TypeScript scaffold

### Решения
| Параметр | Значение |
|---|---|
| `productName` / заголовок окна | `ATLAS` |
| `identifier` | `com.inlerno.atlas` (определяет `%APPDATA%\com.inlerno.atlas`; менять после первого релиза нельзя без миграции данных) |
| npm-пакет / Cargo-крейт | `atlas` / lib `atlas_lib` |
| Пакетный менеджер | npm (`package-lock.json` коммитится) |
| Целевые платформы | Windows 11 x64 — основная; Windows 10 22H2 x64 — best-effort |

### Генерация шаблона
Флаги взяты из `create-tauri-app --help` (npm-пакет `create-tauri-app@4.7.5`; сам CLI сообщил версию 4.8.0):

```powershell
npm create tauri-app@latest -- atlas --manager npm --template react-ts --identifier com.inlerno.atlas --tauri-version 2 --yes
```

Шаблон сгенерирован во временной папке и перенесён в репозиторий без `README.md` и корневого `.gitignore` (последний слит вручную).

### Отклонения от шаблона
- `productName` и заголовок окна: `atlas` → `ATLAS`.
- `Cargo.toml`: заполнены `description` и `authors`.
- `[profile.release] panic = "abort"` **удалён**: при abort паника в задаче опционального модуля завершает весь процесс, что противоречит требованию «сбой необязательного модуля не завершает приложение».
- `.gitignore`: `.vscode/` заменён на `.vscode/*` + `!.vscode/extensions.json`; добавлены `dist-ssr/`, `*.local`.
- Добавлены `.gitattributes` (LF) и `.editorconfig`.
- `Cargo.toml`: `rust-version = "1.90"` — минимальная версия Rust для графа зависимостей Tauri 2.12 (`tauri-utils`, `tauri-runtime-wry`, `tray-icon`).
- **`tauri-plugin-opener` удалён** (Rust-крейт, npm-пакет `@tauri-apps/plugin-opener`, `.plugin(...)` в `lib.rs`, разрешение `opener:default`) по итогам ревью PR #2: плагин выдавал неиспользуемое право `reveal_item_in_dir`, а JS-пакет нигде не импортировался. Плагин неявно обслуживал ссылки `target="_blank"` в демо-UI, поэтому логотипы в `App.tsx` оставлены без ссылок — иначе клик уводил бы webview на внешний сайт. Из `Cargo.lock` ушли 42 пакета его поддерева; версии остальных не менялись.
- `index.html`: заголовок `ATLAS`.

Сохранено из шаблона без изменений (кандидаты на пересмотр в следующих issue):
- демо-команда `greet` и демо-UI (удаляются в #12);
- `"csp": null` — CSP нужно задать до появления сетевых функций.

### Версии (из `npm run tauri info`)
| Пакет | Версия |
|---|---|
| tauri | 2.12.5 |
| tauri-build | 2.7.4 |
| wry / tao | 0.57.1 / 0.37.2 |
| @tauri-apps/api | 2.12.4 |
| @tauri-apps/cli | 2.12.4 |
| react / react-dom | 19.3.1 |
| vite | 8.3.6 |
| @vitejs/plugin-react | 6.2.1 |
| typescript | 6.0.3 |

Версии npm-пакетов — фактические из `package-lock.json`. Минимальные требования: Node.js `^20.19.0 || >=22.12.0` (Vite 8), Rust 1.90+.

### Проверки в облачном окружении Claude (Linux, НЕ Windows)
Окружение: Ubuntu 24.04 x86_64, rustc 1.97.0, cargo 1.97.0, node 22.23.4, npm 10.9.9.

| Команда | Результат |
|---|---|
| `npm install` | OK, 0 vulnerabilities |
| `npm run build` (`tsc && vite build`) | OK |
| `cargo fmt --check` | OK |
| `cargo clippy --all-targets -- -D warnings` | OK |
| `cargo test` | OK (0 тестов) |

Эти проверки подтверждают только, что код компилируется. Запуск окна (`npm run tauri dev`) там не выполнялся.

Повторно после правок по ревью (2026-11-09, та же среда): `npm ci`, `npm run build`, `npm audit` (0), `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked` — OK; `npm run tauri info` — раздел Plugins пуст; capability `default` содержит только `core:default` и проходит проверку `tauri-build`. Компиляция под `x86_64-pc-windows-msvc` из облака невозможна (сетевая политика окружения не даёт скачать rust-std для этого таргета) — её закроет CI в #5.

### Ручная проверка на Windows — заполняет разработчик
> **Статус: ОТЛОЖЕНА** — решение пользователя от 2026-11-09 (допускается правилами `CLAUDE.md` / ADR-010).
> Причина: сейчас нет доступа к Windows-машине.
> Проверка будет выполнена в два этапа:
> 1. компиляция, линковка и тесты под `x86_64-pc-windows-msvc` — CI на `windows-latest` (#5);
> 2. запуск окна и все пункты ниже — на реальной Windows-машине, когда появится доступ.
>
> До выполнения обоих этапов нельзя утверждать, что приложение собирается и работает на Windows. После каждого этапа этот статус обновляется.
>
> **Этап 1 — выполнен (2026-11-09):** Windows CI на `windows-latest` собрал фронтенд, прошёл `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` и `cargo test --locked` на коде PR #2 — прогоны на `9d5f205` и `904c900` (см. запись Issue #5 выше).
> **Этап 2 — отложен:** запуск окна и пункты ниже на реальной Windows-машине.

- [ ] Windows: редакция / версия / сборка:
- [ ] `git --version`:
- [ ] `node --version` / `npm --version`:
- [ ] `rustup show` (toolchain должен быть `*-x86_64-pc-windows-msvc`):
- [ ] Вывод `npm run tauri info` (WebView2, MSVC Build Tools):
- [ ] `npm ci` — OK
- [ ] `npm run tauri dev` — окно «ATLAS» открывается (скриншот в PR)
- [ ] `npm run build` — OK
- [ ] `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` в `src-tauri` — OK
