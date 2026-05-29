from pic_selecter import grouper


def test_default_expert_workers_stays_single_on_cpu(monkeypatch):
    monkeypatch.delenv("PIC_SELECTER_EXPERT_WORKERS", raising=False)
    monkeypatch.setattr("pic_selecter.vision._device_type", lambda: "cpu")
    assert grouper._default_expert_workers() == 1


def test_default_expert_workers_uses_small_parallelism_on_cuda(monkeypatch):
    monkeypatch.delenv("PIC_SELECTER_EXPERT_WORKERS", raising=False)
    monkeypatch.setattr("pic_selecter.vision._device_type", lambda: "cuda")
    workers = grouper._default_expert_workers()
    assert 2 <= workers <= 4


def test_default_expert_workers_respects_env_override(monkeypatch):
    monkeypatch.setenv("PIC_SELECTER_EXPERT_WORKERS", "3")
    monkeypatch.setattr("pic_selecter.vision._device_type", lambda: "cuda")
    assert grouper._default_expert_workers() == 3
