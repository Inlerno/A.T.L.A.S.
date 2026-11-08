# Roadmap

0. **Repository foundation:** GitHub repo, Tauri 2 + React/TS/Vite skeleton, Windows launch verified.
1. **Application foundation:** app shell, typed IPC, Rust state/lifecycle, module registry, SQLite baseline, logging.
2. **Hybrid UI:** sidebar, chat, Orb state machine, themes, settings shell.
3. **LM Studio:** provider adapter, connection test, model list, streaming, cancellation, error states.
4. **Voice:** audio devices, STT/TTS adapters, then VAD and wake-word.
5. **Safe Windows tools:** typed registry, policy, audit, read-only tools first, then confirmed actions.
6. **Memory:** conversation persistence and user-controlled long-term memory.
7. **Telegram:** secure token storage, linking, allowlist, text/voice, audit, rate limits.
8. **Advanced settings/diagnostics:** module control, profiles, health checks, safe export.
9. **Release:** Windows installer/portable evaluation, upgrades, data preservation, clean-machine tests.

Definition of done: acceptance criteria met; automated checks pass; Windows manual verification recorded; errors/limitations explicit; docs updated; no secrets committed.
