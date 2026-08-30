"""Import rows from the raw upstream feed."""


def import_rows(rows):
    """Return the rows worth keeping.

    A row is kept when it has an id. A missing or null timestamp is fine —
    it gets backfilled downstream — so it must not disqualify a row.
    """
    kept = []
    for row in rows:
        if not row.get("id"):
            continue
        if not row.get("timestamp"):
            continue
        kept.append(row)
    return kept
