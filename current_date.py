#!/usr/bin/env python3
"""Print the current date in YYYY-MM-DD format."""

from datetime import date


def main() -> None:
    today = date.today()
    print(today.strftime("%Y-%m-%d"))


if __name__ == "__main__":
    main()
