#!/usr/bin/env python3
"""Exercise SQL extracted from store.rs; this does not execute the Rust application."""
import argparse
import json
import re
import sqlite3
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repository", nargs="?", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    source = (args.repository / "src/store.rs").read_text()
    schema = re.search(r'execute_batch\(\s*"([\s\S]*?)",\s*\)', source).group(1)
    connection = sqlite3.connect(":memory:")
    connection.executescript(schema)
    statements = list(dict.fromkeys(re.findall(r'(?:execute|query_row|prepare)\(\s*"((?:[^"\\]|\\.)*)"', source)))
    for statement in statements:
        count = max([int(value) for value in re.findall(r'\?(\d+)', statement)] or [0])
        connection.execute("EXPLAIN " + statement, [None] * count).fetchall()

    def statement(prefix, contains=None):
        matches = [sql for sql in statements if sql.startswith(prefix) and (contains is None or contains in sql)]
        assert len(matches) == 1, (prefix, matches)
        return matches[0]

    def session(identifier, parent=None):
        info = {"id": identifier, "time": {"created": 1, "updated": 1}}
        connection.execute(statement("INSERT INTO session(id,json,time_created,time_updated,parent_id) VALUES(?1,?2,?3,?4,?5)"), (identifier, json.dumps(info), 1, 1, parent))

    def message(session_id, identifier, role="user", completed=None):
        info = {"id": identifier, "sessionID": session_id, "role": role, "time": {"created": 1}}
        if completed is not None:
            info["time"]["completed"] = completed
        connection.execute(statement("INSERT INTO message("), (identifier, session_id, role, json.dumps(info), 1))

    def part(session_id, message_id, identifier, text):
        info = {"id": identifier, "sessionID": session_id, "messageID": message_id, "type": "text", "text": text}
        connection.execute(statement("INSERT INTO part("), (identifier, message_id, session_id, json.dumps(info), 1))

    session("root")
    session("child", "root")
    session("unrelated")
    message("root", "active")
    message("root", "queued")
    message("child", "child-message")
    message("root", "unfinished", role="assistant")
    message("root", "completed", role="assistant", completed=2)
    rowid = connection.execute("SELECT rowid FROM message WHERE id=?", ("active",)).fetchone()[0]
    message("root", "active")
    assert connection.execute("SELECT rowid FROM message WHERE id=?", ("active",)).fetchone()[0] == rowid
    unfinished = connection.execute(statement("SELECT json FROM message WHERE role='assistant'")).fetchall()
    assert [json.loads(row[0])["id"] for row in unfinished] == ["unfinished"]
    part("root", "active", "part-a", "before")
    part("child", "child-message", "part-b", "child")
    rowid = connection.execute("SELECT rowid FROM part WHERE id=?", ("part-a",)).fetchone()[0]
    part("root", "active", "part-a", "after")
    assert connection.execute("SELECT rowid FROM part WHERE id=?", ("part-a",)).fetchone()[0] == rowid
    for message_id in ("active", "queued"):
        connection.execute(statement("INSERT INTO inbox("), (message_id, "root", json.dumps({"messageID": message_id})))
    sequence = connection.execute(statement("SELECT seq,request FROM inbox"), ("root",)).fetchone()[0]
    connection.execute(statement("UPDATE inbox SET started=1"), (sequence,))
    assert connection.execute(statement("SELECT seq,request FROM inbox"), ("root",)).fetchone()[0] != sequence
    connection.execute(statement("DELETE FROM inbox WHERE started=1"))
    assert connection.execute("SELECT message_id FROM inbox").fetchall() == [("queued",)]
    todo = next(sql for sql in statements if sql.startswith("INSERT INTO todo(") and "ON CONFLICT" in sql)
    connection.execute(todo, ("child", "[]"))
    connection.execute(todo, ("orphan", "[]"))
    connection.executescript(schema)
    assert connection.execute("SELECT session_id FROM todo ORDER BY session_id").fetchall() == [("child",)]
    active_descendants = statement("WITH RECURSIVE descendants", " SELECT EXISTS(")
    assert connection.execute(active_descendants, ("root",)).fetchone() == (1,)
    connection.execute(statement("DELETE FROM inbox WHERE session_id="), ("root",))
    connection.execute(statement("UPDATE session SET busy=?2"), ("child", 1))
    assert connection.execute(active_descendants, ("root",)).fetchone() == (1,)
    connection.execute(statement("UPDATE session SET busy=?2"), ("child", 0))
    assert connection.execute(active_descendants, ("root",)).fetchone() == (0,)
    connection.execute(statement("WITH RECURSIVE descendants", " DELETE FROM session "), ("root",))
    assert connection.execute("SELECT id FROM session").fetchall() == [("unrelated",)]
    for table in ("message", "part", "inbox", "todo"):
        assert connection.execute("SELECT count(*) FROM " + table).fetchone() == (0,), table
    result = {
        "status": "passed",
        "scope": "Actual SQL extracted from src/store.rs, executed with Python SQLite; not Rust compilation or runtime testing",
        "sqlite_version": sqlite3.sqlite_version,
        "prepared_statements": len(statements),
        "checks": ["schema and all SQL syntax", "message/part upsert preserves row order", "recovery selects only unfinished assistant messages", "inbox claim excludes active work", "recovery deletes started work and retains queued work", "schema reopening removes orphan todos", "deletion guard detects queued work and busy descendants", "deletion guard clears after cancellation and idle", "recursive deletion removes parent and descendants", "deletion leaves no message/part/inbox/todo child data", "unrelated session is retained"],
    }
    output = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.write_text(output)
    print(output, end="")


if __name__ == "__main__":
    main()
