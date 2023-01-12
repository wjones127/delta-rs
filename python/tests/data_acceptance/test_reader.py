import json
from pathlib import Path
from typing import Any, Dict, NamedTuple, Optional

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

from deltalake import DeltaTable


class ReadCase(NamedTuple):
    root: Path
    version: Optional[int]
    case_info: Dict[str, Any]
    version_metadata: Dict[str, Any]


cases = []

dat_version = "0.0.1"
reader_case_path = Path("dat-data") / f"v{dat_version}" / "reader_tests" / "generated"

if not reader_case_path.exists():
    pytest.skip(
        "DAT test data not present. Run make setup-dat to download them.",
        allow_module_level=True,
    )

for path in reader_case_path.iterdir():
    if path.is_dir():
        with open(path / "test_case_info.json") as f:
            metadata = json.load(f)

        for version_path in (path / "expected").iterdir():
            if path.name.startswith("v"):
                version = int(path.name[1:])
            else:
                version = None
            with open(version_path / "table_version_metadata.json") as f:
                version_metadata = json.load(f)

        cases.append(ReadCase(path, version, metadata, version_metadata))


@pytest.mark.parametrize(
    "case", cases, ids=lambda case: f"{case.case_info['name']} (version={case.version})"
)
def test_dat(case: ReadCase):
    root, version, case_info, version_metadata = case

    # Get Delta Table path
    delta_root = root / "delta"

    # Load table
    dt = DeltaTable(str(delta_root), version=version)

    # Compare protocol versions
    assert dt.protocol().min_reader_version == version_metadata["min_reader_version"]
    assert dt.protocol().min_writer_version == version_metadata["min_writer_version"]

    # If supported protocol version, try to read, load parquet, and compare
    if dt.protocol().min_reader_version <= 1:
        version_path = "latest" if version is None else f"v{version}"
        # TODO: fix the directory name here
        parquet_root = root / "expected" / version_path / "table_content.parquet"
        expected = pq.read_table(parquet_root)
        actual = dt.to_pyarrow_table()
        assert_tables_equal(expected, actual)
    else:
        # We should raise an error when attempting to read too advanced protocol
        with pytest.raises(Exception):
            dt.to_pyarrow_table()


def assert_tables_equal(first: pa.Table, second: pa.Table) -> None:
    assert first.schema == second.schema
    sort_keys = [(col, "ascending") for col in first.column_names]
    first_sorted = first.sort_by(sort_keys)
    second_sorted = second.sort_by(sort_keys)
    assert first_sorted == second_sorted
