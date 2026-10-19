FROM python:3.12-slim
WORKDIR /app
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY threads_monitor.py .
RUN useradd --uid 10001 --no-create-home app && mkdir /data && chown app /data
USER 10001
ENV PYTHONUNBUFFERED=1 DB_PATH=/data/threads-monitor.db
VOLUME /data
EXPOSE 9090
CMD ["python", "threads_monitor.py"]
