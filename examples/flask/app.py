import platform

from flask import Flask

app = Flask(__name__)


@app.get("/")
def hello():
    return f"hello from fervor ({platform.system()} {platform.release()}, {platform.machine()})\n"


if __name__ == "__main__":
    app.run(host="127.0.0.1", port=5000)
