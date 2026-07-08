from __future__ import annotations

import json
import os
import subprocess
import threading
from pathlib import Path


class EncryptedKeyConfig:
    def __init__(self, path: str, recipient: str, age_key_file: str, known_names: set[str]):
        if path == ":memory:":
            self.path = Path(path)
        else:
            expanded_path = Path(os.path.expanduser(path))
            self.path = expanded_path if expanded_path.is_absolute() else Path.cwd() / expanded_path
        self.recipient = recipient
        self.age_key_file = os.path.expanduser(age_key_file)
        self.known_names = set(known_names)
        self._memory: dict[str, str] = {}
        self._lock = threading.Lock()

    def get_all(self) -> dict[str, str]:
        with self._lock:
            if self._is_memory:
                return dict(self._memory)
            if not self.path.exists():
                return {}
            result = self._run_sops(
                ["decrypt", "--input-type", "json", "--output-type", "json", str(self.path)]
            )
            data = json.loads(result.stdout or "{}")
            if not isinstance(data, dict):
                return {}
            return {
                str(name): str(value)
                for name, value in data.items()
                if name in self.known_names and isinstance(value, str) and value
            }

    def set_values(
        self, values: dict[str, str], delete_names: set[str] | None = None
    ) -> dict[str, str]:
        delete_names = delete_names or set()
        unknown_names = (set(values) | delete_names) - self.known_names
        if unknown_names:
            raise ValueError(f"unknown key name(s): {', '.join(sorted(unknown_names))}")
        with self._lock:
            current = self._memory if self._is_memory else self._read_unlocked()
            for name in delete_names:
                current.pop(name, None)
            for name, value in values.items():
                if value:
                    current[name] = value
            if self._is_memory:
                self._memory = dict(current)
            else:
                self._write_unlocked(current)
            return self._safe_snapshot_from_values(current)

    def set_known_names(self, known_names: set[str]) -> None:
        with self._lock:
            self.known_names = set(known_names)

    def safe_snapshot(self) -> dict[str, dict[str, bool | str]]:
        values = self.get_all()
        return self._safe_snapshot_from_values(values)

    def _safe_snapshot_from_values(
        self, values: dict[str, str]
    ) -> dict[str, dict[str, bool | str]]:
        return {
            name: {
                "configured": name in values,
                "source": "encrypted_file" if name in values else "missing",
            }
            for name in sorted(self.known_names)
        }

    @property
    def _is_memory(self) -> bool:
        return str(self.path) == ":memory:"

    def _read_unlocked(self) -> dict[str, str]:
        if not self.path.exists():
            return {}
        result = self._run_sops(
            ["decrypt", "--input-type", "json", "--output-type", "json", str(self.path)]
        )
        data = json.loads(result.stdout or "{}")
        if not isinstance(data, dict):
            return {}
        return {
            str(name): str(value)
            for name, value in data.items()
            if name in self.known_names and isinstance(value, str) and value
        }

    def _write_unlocked(self, values: dict[str, str]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        plaintext = json.dumps(dict(sorted(values.items())), indent=2) + "\n"
        result = self._run_sops(
            [
                "encrypt",
                "--age",
                self.recipient,
                "--input-type",
                "json",
                "--output-type",
                "json",
                "/dev/stdin",
            ],
            input_text=plaintext,
        )
        tmp_path = self.path.with_name(f".{self.path.name}.tmp")
        tmp_path.write_text(result.stdout, encoding="utf-8")
        tmp_path.replace(self.path)

    def _run_sops(
        self, args: list[str], input_text: str | None = None
    ) -> subprocess.CompletedProcess[str]:
        env = dict(os.environ)
        env["SOPS_AGE_KEY_FILE"] = self.age_key_file
        try:
            return subprocess.run(
                ["sops", *args],
                input=input_text,
                text=True,
                capture_output=True,
                check=True,
                env=env,
            )
        except FileNotFoundError as exc:
            raise RuntimeError("sops command is not installed") from exc
        except subprocess.CalledProcessError as exc:
            message = (exc.stderr or "sops command failed").splitlines()[0]
            raise RuntimeError(message) from exc
