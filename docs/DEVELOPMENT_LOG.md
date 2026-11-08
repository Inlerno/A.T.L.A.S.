# Development log

Фиксирует точные версии окружения и команды, которыми каркас создавался и проверялся. Новые записи добавляются сверху.

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

Результаты прогонов: см. запись ниже (дополняется после прогонов).

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

- [ ] Windows: редакция / версия / сборка:
- [ ] `git --version`:
- [ ] `node --version` / `npm --version`:
- [ ] `rustup show` (toolchain должен быть `*-x86_64-pc-windows-msvc`):
- [ ] Вывод `npm run tauri info` (WebView2, MSVC Build Tools):
- [ ] `npm ci` — OK
- [ ] `npm run tauri dev` — окно «ATLAS» открывается (скриншот в PR)
- [ ] `npm run build` — OK
- [ ] `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` в `src-tauri` — OK
