# Development log

Фиксирует точные версии окружения и команды, которыми каркас создавался и проверялся. Новые записи добавляются сверху.

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
Флаги взяты из `create-tauri-app --help` (create-tauri-app 4.8.0):

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

Сохранено из шаблона без изменений (кандидаты на пересмотр в следующих issue):
- демо-команда `greet` и демо-UI;
- `tauri-plugin-opener` (Rust + JS) и разрешение `opener:default`;
- `"csp": null` — CSP нужно задать до появления сетевых функций.

### Версии (из `npm run tauri info`)
| Пакет | Версия |
|---|---|
| tauri | 2.12.5 |
| tauri-build | 2.7.4 |
| wry / tao | 0.57.1 / 0.37.2 |
| @tauri-apps/api | 2.12.4 |
| @tauri-apps/cli | 2.12.4 |
| tauri-plugin-opener | 2.7.2 |
| react / react-dom | ^19.1.0 |
| vite | ^8.0.16 |
| typescript | ~6.0.3 |

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

### Ручная проверка на Windows — заполняет разработчик
- [ ] Windows: редакция / версия / сборка:
- [ ] `git --version`:
- [ ] `node --version` / `npm --version`:
- [ ] `rustup show` (toolchain должен быть `*-x86_64-pc-windows-msvc`):
- [ ] Вывод `npm run tauri info` (WebView2, MSVC Build Tools):
- [ ] `npm ci` — OK
- [ ] `npm run tauri dev` — окно «ATLAS» открывается (скриншот в PR)
- [ ] `npm run build` — OK
- [ ] `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` в `src-tauri` — OK
