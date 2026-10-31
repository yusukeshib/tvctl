#!/bin/sh
# Builds the agent packages from skills/tvctl/SKILL.md:
#   dist/tvctl-plugin.zip  Agent Plugins 1.0 package for ChatGPT and Codex
#                          (plugin.json at the root, skills/tvctl/SKILL.md)
#   dist/tvctl-skill.zip   the bare skill (tvctl/SKILL.md) for skill uploads
set -eu
cd "$(dirname "$0")/.."

# The plugin version must match the crate version.
crate_version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
plugin_version=$(sed -n 's/.*"version": "\(.*\)".*/\1/p' plugin/plugin.json | head -1)
if [ "$crate_version" != "$plugin_version" ]; then
  echo "plugin/plugin.json version $plugin_version != Cargo.toml version $crate_version" >&2
  exit 1
fi

rm -rf dist/plugin dist/tvctl-plugin.zip dist/tvctl-skill.zip
mkdir -p dist/plugin/skills
cp plugin/plugin.json dist/plugin/
cp -R skills/tvctl dist/plugin/skills/
(cd dist/plugin && zip -qr ../tvctl-plugin.zip . -x '.*' -x '*/.*')
(cd skills && zip -qr ../dist/tvctl-skill.zip tvctl -x '.*' -x '*/.*')
rm -rf dist/plugin
echo "dist/tvctl-plugin.zip"
echo "dist/tvctl-skill.zip"
