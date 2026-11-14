// Minimal editor of module settings (issue #12): a JSON text. Only the JSON
// syntax is checked here; the settings themselves are checked by the Rust
// core, and its answer is shown as is.

import { useCallback, useEffect, useState } from "react";

import { getModuleSettings, toIpcError, updateModuleSettings } from "../../lib/ipc";
import type { ModuleId } from "../../types/modules";

type Message = { kind: "error" | "info"; text: string };

interface Props {
  id: ModuleId;
}

export function SettingsEditor({ id }: Props) {
  const [text, setText] = useState<string | null>(null);
  const [storedValid, setStoredValid] = useState(true);
  const [saving, setSaving] = useState(false);
  const [message, setMessage] = useState<Message | null>(null);

  const load = useCallback(async () => {
    try {
      const settings = await getModuleSettings(id);
      setText(JSON.stringify(settings.value, null, 2));
      setStoredValid(settings.valid);
      setMessage(null);
    } catch (error) {
      setText(null);
      setMessage({ kind: "error", text: toIpcError(error).message });
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  const save = async () => {
    if (text === null) return;
    let value: unknown;
    try {
      value = JSON.parse(text);
    } catch {
      setMessage({ kind: "error", text: "Текст не является корректным JSON." });
      return;
    }
    setSaving(true);
    try {
      // Whether `value` is an acceptable settings object is decided in Rust.
      await updateModuleSettings(id, value as Record<string, unknown>);
      setStoredValid(true);
      setMessage({ kind: "info", text: "Настройки сохранены." });
    } catch (error) {
      setMessage({ kind: "error", text: toIpcError(error).message });
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="settings">
      {text !== null && (
        <>
          {!storedValid && (
            <p className="notice notice-warning">
              Сохранённые настройки не прошли проверку при запуске. Исправьте и сохраните их.
            </p>
          )}
          <textarea
            aria-label={`Настройки модуля ${id} в формате JSON`}
            value={text}
            spellCheck={false}
            rows={Math.min(12, Math.max(4, text.split("\n").length))}
            onChange={(event) => setText(event.target.value)}
            disabled={saving}
          />
          <div className="settings-actions">
            <button type="button" className="primary" onClick={() => void save()} disabled={saving}>
              Сохранить
            </button>
            <button type="button" onClick={() => void load()} disabled={saving}>
              Вернуть сохранённые
            </button>
          </div>
        </>
      )}
      {message && (
        <p className={`notice notice-${message.kind}`} role="status">
          {message.text}
        </p>
      )}
    </div>
  );
}
