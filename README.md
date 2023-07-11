# Fiberplane Daemon

The Fiberplane Daemon allows executing Providers outside of the Studio. This
can be useful in situation where direct access from the Studio to a resource is
not available. It does require that the Daemon has access to the resource in
question.

Unlike a HTTP proxy, this Daemon won't simply forward requests. Rather, it
will invoke a Provider, that will fetch the actual data.

## Quickstart

```shell
cargo install --locked fpd
fpd pull --all
${EDITOR} "$(fpd config paths data-sources)"
```

## Installation

### With cargo

Once the crate is published on crates.io, you will be able to do

```shell
cargo install --locked fpd
```

Otherwise, with a cloned version of the repository

```shell
cargo install --path .
```

### Kubernetes

> Instructions to run on Kubernetes coming soon

## Setup

### Finding configuration directories

To know where the Fiberplane Daemon is looking for its configuration
file (`data_sources.yaml`) and its providers, you can use

```shell
fpd config paths
```

This is where you should put your providers and `data_sources.yaml`
(the exact value depends on the platform).

### Download pre-built providers

To download all first-party (Fiberplane) providers, you can use

```shell
fpd pull
```

Check `fpd pull --help` to see the supported options if you want to pull from a
custom release or branch.

Alternatively, see (Building providers)[#building-providers] below for
information on building (custom) providers.

## Run

Once you the configuration is ready (including the token from `fp` or from Studio
when adding a daemon), you can run it with

```shell
fpd --token $TOKEN
```

You can always check `fpd --help` if you need more guidance

## Overview

The following diagram shows the interaction between the Studio, Daemon (showing
up as "Proxy", its legacy name), and their Providers
([source](https://swimlanes.io/#bZFBEoMgDEX3nCIX8AJOp6u26449AQOxMtOCDaBy+zJqqaA74L//kxDG2MN5qQxUZ1hPPZlBSaQaGvx4tA4kdxxaMu+kgevI+GcHHCJCASS2SqMEpaOEa1QKT7ZY5U5mCjXcDI2c5GJnbH5N8qaH64TCO/xHjMp1QChQDVjYt2UatMaTwHyKEjxVGeg86YU7SCw7i3eB1mZ8zCuxLLX8jx8f59SCQu+Aaxmn21siebCfPZf2WMNF2f7Fw6x/AQ==)):

![](docs/architecture.png)

## Contributing

### Local development

To run the daemon against a custom API, you can use the `--api-base` argument.

### Building providers

To build providers from a local checkout of the [`providers` repository](https://github.com/fiberplane/providers)
you can use the `fpd build-providers` command. Use
`fpd build-providers --help` to see which 
