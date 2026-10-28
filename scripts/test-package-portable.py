"""真实小文件、慢速回环 HTTP 与打包子进程：不下载发行包，不构建 Rust。"""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


spec = importlib.util.spec_from_file_location('portable_builder', Path(__file__).with_name('package-portable.py'))
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


class PackagingResourceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='eve-package-resources-')
        self.root = Path(self.temporary.name)
        self.stopped = threading.Event()
        self.requests = []
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                outer.requests.append(self.path)
                if self.path == '/missing':
                    self.send_error(404)
                    return
                self.send_response(200)
                if self.path == '/slow':
                    self.send_header('Content-Length', '1000')
                elif self.path == '/large-header':
                    self.send_header('Content-Length', '4096')
                elif self.path != '/no-length':
                    self.send_header('Content-Length', '16')
                self.end_headers()
                try:
                    if self.path == '/slow':
                        # 连续有数据，单次 socket timeout 不会触发；父进程总期限仍须终止下载。
                        for _ in range(1000):
                            self.wfile.write(b'x')
                            self.wfile.flush()
                            if outer.stopped.wait(.1):
                                break
                    elif self.path == '/retry' and outer.requests.count('/retry') == 1:
                        self.wfile.write(b'partial')
                    else:
                        self.wfile.write(b'x' * (64 if self.path == '/no-length' else 16))
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    pass

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = 'http://127.0.0.1:' + str(self.server.server_port)

    def tearDown(self):
        self.stopped.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
        self.temporary.cleanup()

    def test_small_download_is_streamed_by_a_real_worker(self):
        output = self.root / 'node-archive'
        builder.worker('download', self.url + '/small', output, timeout=5, capture_output=True)
        self.assertEqual(output.read_bytes(), b'x' * 16)

    def test_continuous_slow_download_has_a_total_deadline(self):
        started = time.monotonic()
        with self.assertRaises(subprocess.TimeoutExpired):
            builder.worker('download', self.url + '/slow', self.root / 'partial', timeout=2, capture_output=True)
        self.assertLess(time.monotonic() - started, 5)
        self.assertIn('/slow', self.requests)

    def test_interrupted_response_retries_from_zero_within_the_same_worker_deadline(self):
        output = self.root / 'retried'
        result = builder.worker('download', self.url + '/retry', output, timeout=5, capture_output=True)
        self.assertEqual(output.read_bytes(), b'x' * 16)
        self.assertEqual(self.requests.count('/retry'), 2)
        self.assertIn(b'"event": "retry"', result.stdout)
        self.assertIn(b'"attempt": 2', result.stdout)

    def test_permanent_http_error_is_not_retried(self):
        with self.assertRaises(subprocess.CalledProcessError):
            builder.worker('download', self.url + '/missing', self.root / 'missing', timeout=5, capture_output=True)
        self.assertEqual(self.requests.count('/missing'), 1)

    def test_download_rejects_oversized_header_before_writing(self):
        output = self.root / 'oversized'
        with self.assertRaises(subprocess.CalledProcessError) as raised:
            builder.worker('download', self.url + '/large-header', output, 32, timeout=5, capture_output=True)
        self.assertIn(b'node_archive_content_length_limit', raised.exception.stderr)
        self.assertEqual(output.stat().st_size, 0)

    def test_download_without_length_still_has_a_byte_limit(self):
        output = self.root / 'unbounded'
        with self.assertRaises(subprocess.CalledProcessError) as raised:
            builder.worker('download', self.url + '/no-length', output, 32, timeout=5, capture_output=True)
        self.assertIn(b'node_archive_bytes_limit', raised.exception.stderr)
        self.assertLessEqual(output.stat().st_size, 32)

    def test_real_archive_and_extraction_roundtrip_for_both_formats(self):
        bundle = self.root / 'bundle'
        bundle.mkdir()
        (bundle / 'example.txt').write_text('合成打包资料', encoding='utf8')
        for extension in ('zip', 'tar.gz'):
            with self.subTest(extension=extension):
                archive = self.root / ('example.' + extension)
                output = self.root / extension
                output.mkdir()
                builder.worker('archive', bundle, archive, timeout=5, capture_output=True)
                builder.worker('extract', archive, output, timeout=5, capture_output=True)
                self.assertEqual((output / 'bundle/example.txt').read_text(encoding='utf8'), '合成打包资料')


if __name__ == '__main__':
    unittest.main()
