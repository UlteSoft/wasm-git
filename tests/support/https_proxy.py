"""Loopback TLS termination for real native integration tests; no mock replies."""
import socket
import socketserver
import ssl
import sys

context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(sys.argv[1], sys.argv[2])
backend_host, backend_port = sys.argv[3].rsplit(':', 1)


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(20)
        try:
            with context.wrap_socket(self.request, server_side=True) as client:
                data = bytearray()
                while b'\r\n\r\n' not in data:
                    part = client.recv(1)
                    if not part:
                        return
                    data.extend(part)
                    if len(data) > 16384:
                        raise ValueError('oversized headers')
                headers = data.decode('ascii').split('\r\n')
                lengths = [int(line.split(':', 1)[1]) for line in headers if line.lower().startswith('content-length:')]
                if len(lengths) != 1 or not 0 <= lengths[0] <= 64 * 1024**2:
                    raise ValueError('invalid request length')
                with socket.create_connection((backend_host, int(backend_port)), timeout=20) as upstream:
                    upstream.sendall(data)
                    remaining = lengths[0]
                    while remaining:
                        part = client.recv(min(65536, remaining))
                        if not part:
                            return
                        remaining -= len(part)
                        upstream.sendall(part)
                    upstream.shutdown(socket.SHUT_WR)
                    while True:
                        part = upstream.recv(65536)
                        if not part:
                            break
                        client.sendall(part)
        except (BrokenPipeError, ConnectionResetError, ssl.SSLError):
            # A deliberately lost reply or an untrusted TLS handshake has no
            # application response to manufacture.
            return


class Server(socketserver.ThreadingTCPServer):
    daemon_threads = True


with Server(('127.0.0.1', 0), Handler) as server:
    print(f'127.0.0.1:{server.server_address[1]}', flush=True)
    server.serve_forever()
