# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

from pathlib import Path
from typing import Any

import click

import materialize.mzexplore as api
import materialize.mzexplore.common as common

# import logging
# logging.basicConfig(encoding='utf-8', level=logging.DEBUG)

# Click CLI Application
# ---------------------


@click.group()
def app() -> None:
    pass


class Arg:
    repository: dict[str, Any] = dict(
        type=click.Path(
            exists=False,
            file_okay=False,
            dir_okay=True,
            writable=True,
            readable=True,
            resolve_path=True,
        ),
        callback=lambda ctx, param, value: Path(value),  # type: ignore
    )

    summary_file: dict[str, Any] = dict(
        type=click.Path(
            exists=False,
            file_okay=True,
            dir_okay=False,
            writable=True,
            readable=True,
            resolve_path=True,
        ),
        callback=lambda ctx, param, value: Path(value),  # type: ignore
    )

    base_suffix: dict[str, Any] = dict(
        type=str,
        metavar="BASE",
    )

    diff_suffix: dict[str, Any] = dict(
        type=str,
        metavar="DIFF",
    )


class Opt:
    db_port: dict[str, Any] = dict(
        default=6877,
        help="DB connection port.",
        envvar="PGPORT",
    )

    db_host: dict[str, Any] = dict(
        default="localhost",
        help="DB connection host.",
        envvar="PGHOST",
    )

    db_user: dict[str, Any] = dict(
        default="mz_support",
        help="DB connection user.",
        envvar="PGUSER",
    )

    db_pass: dict[str, Any] = dict(
        default=None,
        help="DB connection password.",
        envvar="PGPASSWORD",
    )

    db_require_ssl: dict[str, Any] = dict(
        is_flag=True,
        help="DB connection requires SSL.",
        envvar="PGREQUIRESSL",
    )

    mzfmt: dict[str, Any] = dict(
        default=True,
        help="Format SQL statements with `mzfmt` if present.",
    )

    explainee_type: dict[str, Any] = dict(
        type=click.Choice([v.name.lower() for v in list(api.ExplaineeType)]),
        default="all",  # We should have at least arity for good measure.
        callback=lambda ctx, param, v: api.ExplaineeType[v.upper()],  # type: ignore
        help="EXPLAIN mode.",
        metavar="MODE",
    )

    explain_options: dict[str, Any] = dict(
        type=api.ExplainOptionType(),
        multiple=True,
        help="WITH key=val pairs to pass to the EXPLAIN command.",
        metavar="KEY=VAL",
    )

    explain_stage: dict[str, Any] = dict(
        type=click.Choice([str(v.name.lower()) for v in list(api.ExplainStage)]),
        multiple=True,
        default=["optimized_plan"],  # Most often we'll only the optimized plan.
        callback=lambda ctx, param, vals: [api.ExplainStage[v.upper()] for v in vals],  # type: ignore
        help="EXPLAIN stage to export.",
        metavar="STAGE",
    )

    explain_suffix: dict[str, Any] = dict(
        type=str,
        default="",
        help="A suffix to append to the EXPLAIN output files.",
        metavar="SUFFIX",
    )

    explain_format: dict[str, Any] = dict(
        type=click.Choice([str(v.name.lower()) for v in list(api.ExplainFormat)]),
        default="text",
        callback=lambda ctx, param, v: api.ExplainFormat[v.upper()],  # type: ignore
        help="AS [FORMAT] clause to pass to the EXPLAIN command.",
        metavar="FORMAT",
    )


def is_documented_by(original: Any) -> Any:
    def wrapper(target):
        target.__doc__ = original.__doc__
        return target

    return wrapper


@app.group()
@is_documented_by(api.extract)
def extract() -> None:
    pass


@extract.command()
@click.argument("target", **Arg.repository)
@click.argument("database", type=str)
@click.argument("schema", type=str)
@click.argument("name", type=str)
@click.option("--db-port", **Opt.db_port)
@click.option("--db-host", **Opt.db_host)
@click.option("--db-user", **Opt.db_user)
@click.option("--db-pass", **Opt.db_pass)
@click.option("--db-require-ssl", **Opt.db_require_ssl)
@click.option("--mzfmt/--no-mzfmt", **Opt.mzfmt)
@is_documented_by(api.extract.defs)
def defs(
    target: Path,
    database: str,
    schema: str,
    name: str,
    db_port: int,
    db_host: str,
    db_user: str,
    db_pass: str | None,
    db_require_ssl: bool,
    mzfmt: bool,
) -> None:
    try:
        api.extract.defs(
            target,
            database,
            schema,
            name,
            db_port,
            db_host,
            db_user,
            db_pass,
            db_require_ssl,
            mzfmt,
        )
    except Exception as e:
        import traceback

        traceback.print_tb(e.__traceback__)
        raise click.ClickException(f"extract defs command failed: {e=}, {type(e)=}")


@extract.command()
@click.argument("target", **Arg.repository)
@click.argument("database", type=str)
@click.argument("schema", type=str)
@click.argument("name", type=str)
@click.option("--db-port", **Opt.db_port)
@click.option("--db-host", **Opt.db_host)
@click.option("--db-user", **Opt.db_user)
@click.option("--db-pass", **Opt.db_pass)
@click.option("--db-require-ssl", **Opt.db_require_ssl)
@click.option("--explainee-type", "-t", **Opt.explainee_type)
@click.option("--with", "-w", "explain_options", **Opt.explain_options)
@click.option("--stage", "-s", "explain_stages", **Opt.explain_stage)
@click.option("--format", "-f", "explain_format", **Opt.explain_format)
@click.option("--suffix", **Opt.explain_suffix)
@is_documented_by(api.extract.plans)
def plans(
    target: Path,
    database: str,
    schema: str,
    name: str,
    db_port: int,
    db_host: str,
    db_user: str,
    db_pass: str | None,
    db_require_ssl: bool,
    explainee_type: api.ExplaineeType,
    explain_options: list[api.ExplainOption],
    explain_stages: set[api.ExplainStage],
    explain_format: api.ExplainFormat,
    suffix: str | None = None,
) -> None:
    try:
        api.extract.plans(
            target,
            database,
            schema,
            name,
            db_port,
            db_host,
            db_user,
            db_pass,
            db_require_ssl,
            explainee_type,
            explain_options,
            explain_stages,
            explain_format,
            suffix,
        )
    except Exception as e:
        import traceback

        traceback.print_tb(e.__traceback__)
        raise click.ClickException(f"extract plans command failed: {e=}, {type(e)=}")


@app.group()
@is_documented_by(api.analyze)
def analyze() -> None:
    pass


@analyze.command()
@click.argument("target", **Arg.repository)  # type: ignore
@click.argument("summary_file", **Arg.summary_file)  # type: ignore
@click.argument("base_suffix", **Arg.base_suffix)
@click.argument("diff_suffix", **Arg.diff_suffix)
@is_documented_by(api.analyze.changes)
def changes(
    target: Path,
    summary_file: Path,
    base_suffix: str,
    diff_suffix: str,
) -> None:
    """
    Prepare an analysis report of plan changes as a Markdown document.
    """

    try:
        with summary_file.open("a+", encoding="utf-8") as out:
            api.analyze.changes(
                out=out,
                target=target,
                header_name=str(target),
                base_suffix=base_suffix,
                diff_suffix=diff_suffix,
            )
            common.info(f"Summary written to {summary_file}")

    except Exception as e:
        import traceback

        traceback.print_tb(e.__traceback__)
        msg = f"analyze changes command failed: {e=}, {type(e)=}"
        raise click.ClickException(msg) from e


# Entrypoint
# ----------

if __name__ == "__main__":
    app()
