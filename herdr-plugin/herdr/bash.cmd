#!/bin/sh
:; exec bash "$@"
@echo off
rem Every action's command, on every platform: `herdr/bash.cmd <script> <args>`.
rem Herdr takes one command per action id and has no per-platform variant, so
rem this one file is read by two interpreters.
rem
rem On macOS and Linux it is a shell script, and line two runs `bash <script>`,
rem which is what the manifest would say if it could. On Windows Herdr runs a
rem .cmd through cmd, which reads line two as a label and skips it, so the
rem script goes to git-bash.cmd instead. cmd also echoes line one and fails to
rem run it; that lands in `herdr plugin log` and nowhere else.
rem
rem Panes and the build step have no id to share, so on Windows they name
rem git-bash.cmd directly and never show that line.
call "%~dp0git-bash.cmd" %*
exit /b %errorlevel%
