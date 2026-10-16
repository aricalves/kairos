#!/usr/bin/env python3
"""Checks of the skill set of the kairos plugin against its manifest, its
router and its README (the sync rule of KAIROS-A-0014), and of the text that
COLLIERY-T-1866, COLLIERY-T-1864 and COLLIERY-T-1865 put in the skills.
Stdlib only; run with `python3 -m unittest plugin/hooks/test_skills.py` (or
`angreal test unit`, which includes it).

These tests read the files as text. What an agent does with a skill is a
manual check in a real session: the steps are on each task."""

import json
import os
import re
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
PLUGIN = os.path.dirname(HERE)
ROOT = os.path.dirname(PLUGIN)
SKILLS = os.path.join(PLUGIN, "skills")


def read(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def frontmatter(path):
    """The `key: value` lines between the two `---` lines at the top."""
    text = read(path)
    match = re.match(r"---\n(.*?)\n---\n", text, re.S)
    if not match:
        return None
    fields = {}
    for line in match.group(1).splitlines():
        key, sep, value = line.partition(":")
        if sep and not line.startswith(" "):
            fields[key.strip()] = value.strip().strip('"')
    return fields


def skill_dirs():
    """Each folder under plugin/skills/<bucket>/ that has a SKILL.md, as a
    path relative to the repository root."""
    found = []
    for bucket in sorted(os.listdir(SKILLS)):
        bucket_dir = os.path.join(SKILLS, bucket)
        if not os.path.isdir(bucket_dir):
            continue
        for name in sorted(os.listdir(bucket_dir)):
            if os.path.isfile(os.path.join(bucket_dir, name, "SKILL.md")):
                found.append(f"./plugin/skills/{bucket}/{name}")
    return found


def is_user_invoked(fields):
    return fields.get("disable-model-invocation") == "true"


class SkillSetInSync(unittest.TestCase):
    def setUp(self):
        self.manifest = json.loads(read(os.path.join(ROOT, ".claude-plugin", "plugin.json")))
        self.router = read(os.path.join(SKILLS, "meta", "kairos", "SKILL.md"))
        self.readme = read(os.path.join(PLUGIN, "README.md"))
        self.skills = {}
        for rel in skill_dirs():
            path = os.path.join(ROOT, rel, "SKILL.md")
            self.skills[rel] = frontmatter(path)

    def test_manifest_lists_each_skill_folder(self):
        self.assertEqual(sorted(self.manifest["skills"]), sorted(self.skills))

    def test_each_skill_has_a_name_and_a_description(self):
        for rel, fields in self.skills.items():
            with self.subTest(skill=rel):
                self.assertIsNotNone(fields, "no frontmatter")
                self.assertEqual(fields.get("name"), os.path.basename(rel))
                self.assertTrue(fields.get("description"), "no description")

    def test_router_lists_each_user_invoked_skill(self):
        for rel, fields in self.skills.items():
            if is_user_invoked(fields):
                with self.subTest(skill=rel):
                    self.assertIn(f"`/kairos:{fields['name']}`", self.router)

    def test_router_lists_each_model_invoked_skill(self):
        line = next(
            (l for l in self.router.splitlines() if l.startswith("Model-invoked disciplines")),
            "",
        )
        for rel, fields in self.skills.items():
            if not is_user_invoked(fields):
                with self.subTest(skill=rel):
                    self.assertIn(f"`{fields['name']}`", line)

    def test_readme_names_each_skill(self):
        for rel, fields in self.skills.items():
            with self.subTest(skill=rel):
                self.assertIn(fields["name"], self.readme)


class KairosVocabulary(unittest.TestCase):
    """COLLIERY-T-1866."""

    def test_the_skill_is_model_invoked_and_has_its_reference(self):
        folder = os.path.join(SKILLS, "meta", "kairos-vocabulary")
        fields = frontmatter(os.path.join(folder, "SKILL.md"))
        self.assertFalse(is_user_invoked(fields))
        for word in ("task 1", "slice 2", "D3", "epic", "sprint", "mark it done"):
            self.assertIn(word, fields["description"])
        self.assertTrue(os.path.isfile(os.path.join(folder, "ITEM-TYPES.md")))

    def test_decompose_proposes_quoted_titles(self):
        text = read(os.path.join(SKILLS, "workflow", "decompose", "SKILL.md"))
        step4 = text.split("### 4. Quiz the user", 1)[1].split("### 5.", 1)[0]
        self.assertIn("quoted titles", step4)
        self.assertIn('"not yet created"', step4)
        self.assertNotIn("numbered list", step4)
        self.assertNotIn("which other slices", step4)
        self.assertIn("kairos-vocabulary", step4)

    def test_skills_that_name_work_point_to_the_vocabulary(self):
        for rel in ("meta/grilling", "workflow/to-initiative", "workflow/triage"):
            with self.subTest(skill=rel):
                text = read(os.path.join(SKILLS, rel, "SKILL.md"))
                self.assertIn("kairos-vocabulary", text)



class BlocksEdgesWhenWorkSplits(unittest.TestCase):
    """KAIROS-T-0493: a skill that splits work sets a `blocks` edge for each
    task that must wait, and no edge between parallel tasks, so the graph
    view shows serial and parallel work."""

    SKILLS_THAT_SPLIT = (
        "workflow/decompose",
        "workflow/grill-decomposition",
        "workflow/implement",
        "meta/kairos-vocabulary",
    )

    def test_each_skill_that_splits_work_names_the_rule(self):
        for rel in self.SKILLS_THAT_SPLIT:
            with self.subTest(skill=rel):
                text = read(os.path.join(SKILLS, rel, "SKILL.md"))
                self.assertIn("`blocks`", text)
                self.assertIn("parallel", text)
                self.assertIn("no edge", text)

    def test_decompose_writes_the_order_in_the_task(self):
        text = read(os.path.join(SKILLS, "workflow", "decompose", "SKILL.md"))
        template = text.split("<task-template>", 1)[1].split("</task-template>", 1)[0]
        self.assertIn("## Order", template)
        self.assertIn("can go in parallel", template)
        step5 = text.split("### 5.", 1)[1].split("## Reference", 1)[0]
        self.assertIn("graph view", step5)




class GrillInitiative(unittest.TestCase):
    """COLLIERY-T-1864."""

    def setUp(self):
        path = os.path.join(SKILLS, "workflow", "grill-initiative", "SKILL.md")
        self.fields = frontmatter(path)
        self.text = read(path)

    def test_the_skill_is_user_invoked(self):
        self.assertTrue(is_user_invoked(self.fields))

    def test_it_uses_the_grilling_loop_and_writes_as_it_goes(self):
        self.assertIn("`grilling`", self.text)
        self.assertIn("## Decisions (<who>, <date>)", self.text)
        self.assertIn("`edit_item`", self.text)

    def test_it_never_moves_the_item(self):
        self.assertIn("Never call `transition_item`", self.text)

    def test_it_offers_the_next_skills(self):
        self.assertIn("/kairos:decompose", self.text)



class GrillDecomposition(unittest.TestCase):
    """COLLIERY-T-1865."""

    def setUp(self):
        path = os.path.join(SKILLS, "workflow", "grill-decomposition", "SKILL.md")
        self.fields = frontmatter(path)
        self.text = read(path)

    def test_the_skill_is_user_invoked(self):
        self.assertTrue(is_user_invoked(self.fields))

    def test_it_reviews_the_four_questions(self):
        for heading in ("Slice boundaries", "Order and risk", "Done criteria", "Scope and repositories"):
            self.assertIn(heading, self.text)
        self.assertIn("`blocks`", self.text)
        self.assertIn("`set_repository`", self.text)

    def test_it_goes_back_up_and_never_moves_an_item(self):
        self.assertIn("/kairos:grill-initiative", self.text)
        self.assertIn("Never call `transition_item`", self.text)
        self.assertIn("/kairos:ralph-initiative", self.text)

    def test_decompose_offers_it_at_the_end(self):
        text = read(os.path.join(SKILLS, "workflow", "decompose", "SKILL.md"))
        step5 = text.split("### 5.", 1)[1].split("## Reference", 1)[0]
        self.assertIn("/kairos:grill-decomposition", step5)


class NotificationsInTheSkills(unittest.TestCase):
    """KAIROS-T-0382: implement and triage read and mark the events."""

    def test_implement_and_triage_read_and_mark_events(self):
        for rel in ("workflow/implement", "workflow/triage"):
            with self.subTest(skill=rel):
                text = read(os.path.join(SKILLS, rel, "SKILL.md"))
                self.assertIn("`get_events`", text)
                self.assertIn("`mark_events_read`", text)


class TheStateInTheSkills(unittest.TestCase):
    """KAIROS-T-0507: implement writes the live state of the task with
    `set_state` after each push and before each stop; handoff reads it and
    writes it."""

    def test_implement_sets_the_state_after_a_push_and_before_a_stop(self):
        text = read(os.path.join(SKILLS, "workflow", "implement", "SKILL.md"))
        self.assertIn("`set_state`", text)
        for field in ("branch", "commit", "pushed", "pr_url", "next_step"):
            self.assertIn(f"`{field}`", text)
        self.assertIn("before", text.split("`set_state`", 1)[1][:600])

    def test_handoff_reads_and_writes_the_state(self):
        text = read(os.path.join(SKILLS, "meta", "handoff", "SKILL.md"))
        self.assertIn("`set_state`", text)
        self.assertIn("state", text.split("`get_item`", 1)[0])


class BootstrapWritesNoRepositoryConnection(unittest.TestCase):
    """The MCP connection belongs to the user: the plugin server or a
    user-scope entry. A project `.mcp.json` only when the user asks."""

    def setUp(self):
        self.text = read(os.path.join(SKILLS, "meta", "bootstrap", "SKILL.md"))

    def test_the_oauth_path_adds_a_user_scope_entry(self):
        self.assertIn("claude mcp add --scope user", self.text)
        self.assertNotRegex(self.text, r"claude mcp add --transport")

    def test_a_project_entry_is_only_on_request(self):
        self.assertNotIn("propose the project `.mcp.json`", self.text)
        self.assertIn("Only when the user asks for this repository only", self.text)

    def test_the_plugin_ships_no_project_template(self):
        self.assertFalse(os.path.exists(os.path.join(PLUGIN, ".mcp.json")))


if __name__ == "__main__":
    unittest.main()
