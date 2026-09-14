# A CONFIG.JSON naming its own Codex model list, so the picker shows a slug
# no build of nebula has ever heard of beside the built-in ones.
mkdir -p "$WORK/data"
cat > "$WORK/data/config.json" <<'JSON'
{
  "codex_models": ["gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-6-astra"]
}
JSON
