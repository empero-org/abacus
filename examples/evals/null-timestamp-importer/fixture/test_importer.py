from importer import import_rows


def test_keeps_rows_without_a_timestamp():
    rows = [
        {"id": 1, "timestamp": "2026-01-01T00:00:00Z"},
        {"id": 2, "timestamp": None},
        {"id": 3},
    ]
    assert [row["id"] for row in import_rows(rows)] == [1, 2, 3]


def test_still_drops_rows_without_an_id():
    rows = [
        {"id": None, "timestamp": "2026-01-01T00:00:00Z"},
        {"timestamp": None},
    ]
    assert import_rows(rows) == []


if __name__ == "__main__":
    test_keeps_rows_without_a_timestamp()
    test_still_drops_rows_without_an_id()
    print("ok")
