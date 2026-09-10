#!/usr/bin/env python3
"""PTY memory menu tests; cached local setup is optional and runs with dead proxies."""
import os
from pathlib import Path
import sys
import tempfile
import tomllib
from terminal_smoke import Terminal


def synchronized_mark(term):
    """Drain trailing repaint frames so a later expect cannot match an old menu."""
    for _ in range(20):
        before = len(term.output)
        term.drain(0.02)
        if len(term.output) == before:
            break
    return len(term.output)


binary = Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/builder').resolve()
assets = os.environ.get('BUILDER_LOCAL_MODEL_DIR')
for plain in (False, True):
    with tempfile.TemporaryDirectory(prefix='builder-memory-menu-') as directory:
        root = Path(directory)
        home = root / 'home'
        home.mkdir()
        path = home / 'config.toml'
        path.write_text('default_profile = "local"\n[profiles.local]\nbase_url = "http://127.0.0.1:1/v1"\nmodel = "menu-test"\n')
        if assets:
            cache = home / 'models/all-MiniLM-L6-v2'
            cache.mkdir(parents=True)
            for name in ['model.onnx','config.json','tokenizer.json','tokenizer_config.json','special_tokens_map.json']:
                os.link(Path(assets) / name, cache / name)
        os.environ.update(HTTPS_PROXY='http://127.0.0.1:1', HTTP_PROXY='http://127.0.0.1:1', ALL_PROXY='http://127.0.0.1:1')
        term = Terminal(binary, home, root, args=('--plain',) if plain else ())
        ready = 'builder ›' if plain else 'Ask Builder'
        try:
            term.expect(ready, timeout=20)
            mark = synchronized_mark(term)
            term.send('/memory\r')
            term.expect('Keyword-only memory', mark)
            term.send('2\r')
            term.expect('Memory settings saved and applied', mark)
            term.expect(ready, term.output.find(b'Memory settings saved and applied', mark) + len(b'Memory settings saved and applied'))
            assert tomllib.loads(path.read_text())['memory']['embedding_backend'] == 'lexical'
            mark = synchronized_mark(term)
            term.send('/memory\r')
            term.expect('Keyword-only memory', mark)
            term.send('3\r')
            term.expect('Memory settings saved and applied', mark)
            term.expect(ready, term.output.find(b'Memory settings saved and applied', mark) + len(b'Memory settings saved and applied'))
            assert not tomllib.loads(path.read_text())['memory']['enabled']
            if assets:
                mark = synchronized_mark(term)
                term.send('/memory\r')
                term.expect('Keyword-only memory', mark)
                term.send('1\r')
                term.expect('Preparing local embeddings', mark)
                # Another settings writer must not lose an unrelated profile edit.
                path.write_text(path.read_text().replace('model = "menu-test"', 'model = "concurrent-edit"'))
                term.expect('Memory settings saved and applied', mark, timeout=60)
                term.expect(ready, term.output.find(b'Memory settings saved and applied', mark) + len(b'Memory settings saved and applied'))
                config = tomllib.loads(path.read_text())
                assert config['memory']['embedding_backend'] == 'local'
                assert config['memory']['enabled']
                assert config['profiles']['local']['model'] == 'concurrent-edit'
                # A conflicting memory update must be rejected, not overwritten.
                mark = synchronized_mark(term)
                term.send('/memory\r')
                term.expect('Keyword-only memory', mark)
                term.send('1\r')
                term.expect('Preparing local embeddings', mark)
                path.write_text(path.read_text().replace('embedding_backend = "local"','embedding_backend = "lexical"'))
                term.expect('Memory settings changed during setup', mark, timeout=60)
                assert tomllib.loads(path.read_text())['memory']['embedding_backend'] == 'lexical'
        finally:
            term.close()
        print('PASS:', 'plain' if plain else 'raw', 'memory menu; cached offline setup/conflicts' if assets else 'memory modes')
