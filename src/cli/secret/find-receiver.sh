# Run in the remote account's noninteractive shell via sh -c.
# Probes get EOF so only the selected receiver can read the credential payload.
for receiver in "$(command -v fridica)" \
    "${VIRTUAL_ENV:-$HOME/pyenv}/bin/fridica" "$HOME/pyenv/bin/fridica" \
    "$HOME/.local/bin/fridica" "$HOME/.cargo/bin/fridica" \
    "$HOME/.venv/bin/fridica" "$HOME/venv/bin/fridica" \
    "$HOME/.local/share/pipx/venvs/fridica/bin/fridica" \
    "$HOME/.pyenv/shims/fridica" "$HOME/.pyenv/versions"/*/bin/fridica \
    "$HOME/.virtualenvs"/*/bin/fridica "$HOME/.conda/envs"/*/bin/fridica \
    "$HOME/miniconda3/bin/fridica" "$HOME/miniconda3/envs"/*/bin/fridica \
    "$HOME/anaconda3/bin/fridica" "$HOME/anaconda3/envs"/*/bin/fridica \
    /opt/homebrew/bin/fridica /usr/local/bin/fridica /usr/bin/fridica
do
    if [ -f "$receiver" ] && [ -x "$receiver" ] &&
        "$receiver" secret receive --help </dev/null >/dev/null 2>&1
    then
        exec "$receiver" secret receive
    fi
done
echo 'No compatible Fridica receiver found on PATH or in common install locations. Use --remote-bin with its absolute path.' >&2
exit 127
