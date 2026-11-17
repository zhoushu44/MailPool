import socket

for host in ["api.github.com", "github.com", "codeload.github.com",
             "objects.githubusercontent.com", "hub.docker.com"]:
    try:
        socket.create_connection((host, 443), timeout=8)
        print("OK   ", host)
    except Exception as e:
        print("FAIL ", host, str(e)[:70])
