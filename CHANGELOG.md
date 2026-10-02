# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This file records what users and packagers see change. Build scaffolding, test
infrastructure, and contributor tooling do not appear here; the commit log and the specs
they name carry that history.

## [Unreleased]

- Query namespace-bound interface state and execute explicit Ethernet/veth MTU,
  administrative-state, and IPv4 operations with dry-run validation and
  per-operation failure reports (core/004-network-backend).

- Decode YAML values from their schema path instead of globally inferring IP
  types from string contents (core/002-schema-validation).
