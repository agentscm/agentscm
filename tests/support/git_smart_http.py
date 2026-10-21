"""Synthetic loopback TLS Git smart-HTTP fixture; runtime secrets are never logged."""
import base64
import http.server
import json
import os
import ssl
import subprocess
import sys
from pathlib import Path

config_path = Path(sys.argv[1])
config = json.loads(config_path.read_text())

class GitHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.0'

    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.dispatch()

    def do_POST(self):
        self.dispatch()

    def dispatch(self):
        settings = json.loads(config_path.read_text())
        with open(settings['requests'], 'a', encoding='utf-8') as log:
            log.write(json.dumps({'method': self.command, 'path': self.path}) + '\n')
        credential = json.loads(Path(settings['credential']).read_text())
        expected = 'Basic ' + base64.b64encode((credential['username'] + ':' + credential['password']).encode()).decode()
        if self.headers.get('Authorization') != expected:
            self.send_response(401)
            self.send_header('WWW-Authenticate', 'Basic realm="synthetic"')
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        if settings.get('redirect'):
            self.send_response(302)
            self.send_header('Location', settings['redirect'])
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        body = bytearray()
        if self.headers.get('Transfer-Encoding', '').lower() == 'chunked':
            while True:
                count = int(self.rfile.readline(128).strip().split(b';')[0], 16)
                if count == 0:
                    self.rfile.readline(128)
                    break
                if len(body) + count > 32 * 1024 * 1024:
                    raise ValueError('fixture request exceeds bound')
                body.extend(self.rfile.read(count))
                if self.rfile.read(2) != b'\r\n':
                    raise ValueError('invalid chunk')
        else:
            count = int(self.headers.get('Content-Length', '0'))
            if count > 32 * 1024 * 1024:
                raise ValueError('fixture request exceeds bound')
            body.extend(self.rfile.read(count))
        path, _, query = self.path.partition('?')
        environment = {
            'PATH': '/usr/bin:/bin', 'HOME': settings['home'],
            'GIT_CONFIG_GLOBAL': settings['global_config'],
            'GIT_CONFIG_SYSTEM': settings['system_config'],
            'GIT_PROJECT_ROOT': settings['root'], 'GIT_HTTP_EXPORT_ALL': '1',
            'PATH_INFO': path, 'QUERY_STRING': query,
            'REQUEST_METHOD': self.command, 'REMOTE_USER': 'synthetic',
            'CONTENT_TYPE': self.headers.get('Content-Type', ''),
            'CONTENT_LENGTH': str(len(body)),
        }
        # Synthetic race injection used only by attached-lifecycle fixtures.
        if self.command == 'POST' and path.endswith('/git-receive-pack') and settings.get('before_push_write'):
            change = settings['before_push_write']
            Path(change['path']).write_text(change['text'], encoding='utf-8')
        result = subprocess.run([settings['git'], 'http-backend'], input=body, capture_output=True, env=environment, timeout=15)
        if result.returncode != 0:
            self.send_error(500, 'synthetic backend failed')
            return
        if self.command == 'POST' and path.endswith('/git-receive-pack') and settings.get('lose_push_reply'):
            self.close_connection = True
            return
        headers, separator, payload = result.stdout.partition(b'\r\n\r\n')
        if not separator:
            headers, _, payload = result.stdout.partition(b'\n\n')
        status = 200
        fields = []
        for line in headers.splitlines():
            key, value = line.decode('ascii').split(':', 1)
            if key.lower() == 'status':
                status = int(value.strip().split()[0])
            else:
                fields.append((key, value.strip()))
        self.send_response(status)
        for key, value in fields:
            self.send_header(key, value)
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), GitHandler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(config['certificate'], config['key'])
server.socket = context.wrap_socket(server.socket, server_side=True)
port_file = Path(config['port_file'])
ready_file = port_file.with_suffix('.ready')
ready_file.write_text(str(server.server_port))
ready_file.replace(port_file)
server.serve_forever()
