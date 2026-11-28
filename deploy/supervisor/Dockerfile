FROM python:3.13-slim
RUN useradd --system --uid 10002 --no-create-home supervisor
COPY supervisor.py /usr/local/bin/neha-supervisor
RUN chmod 0555 /usr/local/bin/neha-supervisor
# The Docker socket is root-owned; the Supervisor necessarily has host-root
# authority over workers. It publishes no port and is only on the internal
# network.
ENTRYPOINT ["python3", "/usr/local/bin/neha-supervisor"]
