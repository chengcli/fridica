import pytest

from fridica.core.errors import MatchError
from fridica.machines import Selector, resolve


def test_explicit_machine_wins(config):
    placement = resolve(config.machines, Selector(machine="snowy", workspace="exocubed"), sticky_machine="dart9")
    assert (placement.machine.name, placement.workspace.name, placement.backend) == ("snowy", "exocubed", "codex")


def test_tags_pick_a_capable_machine_preferring_the_sticky_one_then_the_least_busy(config):
    assert resolve(config.machines, Selector(tags=("rtx5090",), workspace="canoe")).machine.name == "snowy"
    assert resolve(config.machines, Selector(tags=("cuda",), workspace="canoe"), sticky_machine="dart9").machine.name == "dart9"
    busy = {"snowy": 2, "dart9": 0}
    assert resolve(config.machines, Selector(tags=("cuda",), workspace="canoe"), busy=busy).machine.name == "dart9"


def test_no_selector_uses_sticky_machine_and_workspace_then_default(config):
    placement = resolve(config.machines, Selector(), sticky_machine="snowy", sticky_workspace="canoe")
    assert (placement.machine.name, placement.workspace.name) == ("snowy", "canoe")
    assert resolve(config.machines, Selector()).machine.name == "local"


def test_a_workspace_on_exactly_one_machine_moves_the_placement(config):
    assert resolve(config.machines, Selector(workspace="exocubed")).machine.name == "snowy"


def test_ambiguity_and_unknowns_are_errors_with_candidates(config):
    with pytest.raises(MatchError) as error:
        resolve(config.machines, Selector(workspace="canoe"))
    assert set(error.value.candidates) == {"snowy", "dart9"}
    with pytest.raises(MatchError, match="several workspaces") as error:
        resolve(config.machines, Selector(machine="snowy"))
    assert error.value.candidates == ("exocubed", "canoe")
    with pytest.raises(MatchError, match="unknown machine"):
        resolve(config.machines, Selector(machine="mars"))
    with pytest.raises(MatchError, match="lacks rtx5090"):
        resolve(config.machines, Selector(machine="dart9", tags=("rtx5090",)))
    with pytest.raises(MatchError, match="no claude backend"):
        resolve(config.machines, Selector(machine="dart9", backend="claude"))
    with pytest.raises(MatchError, match="no machine has"):
        resolve(config.machines, Selector(tags=("tpu",)))


def test_gpus_are_split_across_job_slots():
    from fridica.machines.registry import slot_gpus
    assert slot_gpus((0, 1), 1, 2) == (0,) and slot_gpus((0, 1), 2, 2) == (1,)
    assert slot_gpus((0, 1, 2, 3), 2, 2) == (2, 3)
    assert slot_gpus((0, 1), 3, 3) == (0,)          # fewer GPUs than slots: shared round robin
    assert slot_gpus((0, 1), 1, 1) == (0, 1) and slot_gpus(None, 1, 2) is None and slot_gpus((0, 1), 0, 2) == (0, 1)


def test_slot_views_of_machines_and_workspaces(config):
    from dataclasses import replace
    snowy = replace(config.machines["snowy"], max_jobs=2)
    assert snowy.for_slot(1).resources.gpus == (0,) and snowy.for_slot(1).resources.cpus == 32
    canoe = snowy.workspace("canoe")
    assert str(canoe.for_slot(2).path) == "/home/me/canoe/worker2" and canoe.for_slot(0) is canoe  # on by default
    whole = replace(canoe, subfolders=False)
    assert whole.for_slot(2) is whole
