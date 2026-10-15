from models import Store


def run(store: Store) -> str:
    return store.save("x")
