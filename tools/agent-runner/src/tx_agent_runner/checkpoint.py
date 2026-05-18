"""SQLite-backed checkpoint saver for cross-process persistence.

Usage::

    from tx_agent_runner.checkpoint import SqliteSaver

    saver = SqliteSaver.from_conn_string("tx_agent.db")
    graph = builder.compile(checkpointer=saver)
"""

from __future__ import annotations

import sqlite3
import threading
from contextlib import contextmanager
from typing import Any, Iterator

from langgraph.checkpoint.base import (
    BaseCheckpointSaver,
    Checkpoint,
    CheckpointMetadata,
    CheckpointTuple,
    ChannelVersions,
)


class SqliteSaver(BaseCheckpointSaver[str]):
    """Persist graph checkpoints to a SQLite database.

    Thread-safe. Survives process restarts.
    """

    def __init__(self, conn: sqlite3.Connection) -> None:
        super().__init__()
        self._conn = conn
        self._lock = threading.Lock()
        self._ensure_schema()

    @staticmethod
    def _dump(obj: Any) -> bytes:
        """Serialize to a SQLite-storable blob.

        Returns a JSON-encoded dict ``{"t": type_hint, "d": base64_data}``.
        """
        import base64, json
        from langgraph.checkpoint.serde.jsonplus import JsonPlusSerializer
        serde = JsonPlusSerializer()
        type_hint, data = serde.dumps_typed(obj)
        return json.dumps({"t": type_hint, "d": base64.b64encode(data).decode()}).encode()

    @staticmethod
    def _load(blob: bytes) -> Any:
        """Deserialize a blob produced by :meth:`_dump`."""
        import base64, json
        from langgraph.checkpoint.serde.jsonplus import JsonPlusSerializer
        serde = JsonPlusSerializer()
        wrapper = json.loads(blob)
        data = base64.b64decode(wrapper["d"])
        return serde.loads_typed((wrapper["t"], data))

    @classmethod
    def from_conn_string(cls, path: str, /) -> SqliteSaver:
        conn = sqlite3.connect(path, check_same_thread=False)
        return cls(conn)

    # ------------------------------------------------------------------ Schema

    def _ensure_schema(self) -> None:
        with self._cursor() as cur:
            cur.executescript("""
                CREATE TABLE IF NOT EXISTS checkpoints (
                    thread_id       TEXT NOT NULL,
                    checkpoint_ns   TEXT NOT NULL DEFAULT '',
                    checkpoint_id   TEXT NOT NULL,
                    parent_id       TEXT,
                    checkpoint      BLOB NOT NULL,
                    metadata        BLOB NOT NULL,
                    PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id)
                );

                CREATE TABLE IF NOT EXISTS writes (
                    thread_id       TEXT NOT NULL,
                    checkpoint_ns   TEXT NOT NULL DEFAULT '',
                    checkpoint_id   TEXT NOT NULL,
                    task_id         TEXT NOT NULL,
                    idx             INTEGER NOT NULL,
                    channel         TEXT NOT NULL,
                    value           BLOB NOT NULL,
                    PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id, task_id, idx)
                );
            """)
            self._conn.commit()

    # ------------------------------------------------------------------ Read

    def get_tuple(self, config: dict[str, Any]) -> CheckpointTuple | None:
        thread_id = config["configurable"]["thread_id"]
        checkpoint_ns = config["configurable"].get("checkpoint_ns", "")
        checkpoint_id = config["configurable"].get("checkpoint_id")

        with self._cursor() as cur:
            if checkpoint_id:
                cur.execute(
                    "SELECT checkpoint, metadata, parent_id FROM checkpoints "
                    "WHERE thread_id=? AND checkpoint_ns=? AND checkpoint_id=?",
                    (thread_id, checkpoint_ns, checkpoint_id),
                )
            else:
                cur.execute(
                    "SELECT checkpoint, metadata, parent_id FROM checkpoints "
                    "WHERE thread_id=? AND checkpoint_ns=? ORDER BY rowid DESC LIMIT 1",
                    (thread_id, checkpoint_ns),
                )
            row = cur.fetchone()

        if row is None:
            return None

        ckpt_bytes, meta_bytes, parent_id = row
        checkpoint: Checkpoint = self._load(ckpt_bytes)
        metadata: CheckpointMetadata = self._load(meta_bytes)
        pending_writes = self._load_writes(thread_id, checkpoint_ns, checkpoint["id"])

        parent_config = None
        if parent_id:
            parent_config = {
                "configurable": {
                    "thread_id": thread_id,
                    "checkpoint_ns": checkpoint_ns,
                    "checkpoint_id": parent_id,
                }
            }

        return CheckpointTuple(
            config={
                "configurable": {
                    "thread_id": thread_id,
                    "checkpoint_ns": checkpoint_ns,
                    "checkpoint_id": checkpoint["id"],
                }
            },
            checkpoint=checkpoint,
            metadata=metadata,
            parent_config=parent_config,
            pending_writes=pending_writes,
        )

    # ------------------------------------------------------------------ Write

    def put(
        self,
        config: dict[str, Any],
        checkpoint: Checkpoint,
        metadata: CheckpointMetadata,
        new_versions: ChannelVersions,
    ) -> dict[str, Any]:
        thread_id = config["configurable"]["thread_id"]
        checkpoint_ns = config["configurable"].get("checkpoint_ns", "")
        parent_id = config["configurable"].get("checkpoint_id")

        with self._cursor() as cur:
            cur.execute(
                "INSERT OR REPLACE INTO checkpoints "
                "(thread_id, checkpoint_ns, checkpoint_id, parent_id, checkpoint, metadata) "
                "VALUES (?, ?, ?, ?, ?, ?)",
                (
                    thread_id,
                    checkpoint_ns,
                    checkpoint["id"],
                    parent_id,
                    self._dump(checkpoint),
                    self._dump(metadata),
                ),
            )
            self._conn.commit()

        return {
            "configurable": {
                "thread_id": thread_id,
                "checkpoint_ns": checkpoint_ns,
                "checkpoint_id": checkpoint["id"],
            }
        }

    def put_writes(
        self,
        config: dict[str, Any],
        writes: list[tuple[str, Any]],
        task_id: str,
    ) -> dict[str, Any]:
        thread_id = config["configurable"]["thread_id"]
        checkpoint_ns = config["configurable"].get("checkpoint_ns", "")
        checkpoint_id = config["configurable"].get("checkpoint_id", "")

        with self._cursor() as cur:
            for idx, (channel, value) in enumerate(writes):
                cur.execute(
                    "INSERT OR REPLACE INTO writes "
                    "(thread_id, checkpoint_ns, checkpoint_id, task_id, idx, channel, value) "
                    "VALUES (?, ?, ?, ?, ?, ?, ?)",
                    (
                        thread_id,
                        checkpoint_ns,
                        checkpoint_id,
                        task_id,
                        idx,
                        channel,
                        self._dump(value),
                    ),
                )
            self._conn.commit()

        return config

    # ------------------------------------------------------------------ List

    def list(
        self,
        config: dict[str, Any] | None,
        *,
        filter: dict[str, Any] | None = None,
        before: dict[str, Any] | None = None,
        limit: int | None = None,
    ) -> Iterator[CheckpointTuple]:
        if config is None:
            return

        thread_id = config["configurable"]["thread_id"]
        checkpoint_ns = config["configurable"].get("checkpoint_ns", "")

        query = (
            "SELECT checkpoint, metadata, parent_id FROM checkpoints "
            "WHERE thread_id=? AND checkpoint_ns=? ORDER BY rowid DESC"
        )
        params: list[Any] = [thread_id, checkpoint_ns]
        if limit is not None:
            query += " LIMIT ?"
            params.append(limit)

        with self._cursor() as cur:
            cur.execute(query, params)
            rows = cur.fetchall()

        for ckpt_bytes, meta_bytes, parent_id in rows:
            checkpoint = self._load(ckpt_bytes)
            metadata = self._load(meta_bytes)
            pending_writes = self._load_writes(thread_id, checkpoint_ns, checkpoint["id"])
            parent_config = None
            if parent_id:
                parent_config = {
                    "configurable": {
                        "thread_id": thread_id,
                        "checkpoint_ns": checkpoint_ns,
                        "checkpoint_id": parent_id,
                    }
                }
            yield CheckpointTuple(
                config={
                    "configurable": {
                        "thread_id": thread_id,
                        "checkpoint_ns": checkpoint_ns,
                        "checkpoint_id": checkpoint["id"],
                    }
                },
                checkpoint=checkpoint,
                metadata=metadata,
                parent_config=parent_config,
                pending_writes=pending_writes,
            )

    # ------------------------------------------------------------------ Internal

    def _load_writes(
        self, thread_id: str, checkpoint_ns: str, checkpoint_id: str
    ) -> list[tuple[str, str, Any]]:
        with self._cursor() as cur:
            cur.execute(
                "SELECT task_id, channel, value FROM writes "
                "WHERE thread_id=? AND checkpoint_ns=? AND checkpoint_id=? "
                "ORDER BY task_id, idx",
                (thread_id, checkpoint_ns, checkpoint_id),
            )
            rows = cur.fetchall()
        return [(tid, ch, self.serde.loads_typed(val)) for tid, ch, val in rows]

    @contextmanager
    def _cursor(self) -> Iterator[sqlite3.Cursor]:
        with self._lock:
            cur = self._conn.cursor()
            try:
                yield cur
            finally:
                cur.close()

    def __enter__(self) -> SqliteSaver:
        return self

    def __exit__(self, *args: Any) -> None:
        self._conn.close()
