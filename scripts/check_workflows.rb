#!/usr/bin/env ruby
# frozen_string_literal: true

# Structurally validate GitHub Actions workflows using Ruby's standard Psych
# parser. This deliberately rejects duplicate keys and aliases before loading,
# then evaluates the same decoded mapping GitHub evaluates (including quoted,
# escaped, explicit, tagged, and flow-style keys).

require "pathname"
require "psych"
require "tempfile"

ROOT = Pathname.new(__dir__).parent.realpath
WORKFLOW_DIR = ROOT.join(".github", "workflows")
HOSTED_RUNNERS = %w[
  ubuntu-24.04
  macos-15
  macos-15-intel
  windows-2025
].freeze
RELEASE_PERMISSIONS = {
  "contents" => "write",
  "id-token" => "write",
  "attestations" => "write"
}.freeze
PINNED_ACTION = %r{\A[^/@\s]+/[^/@\s]+@[0-9a-f]{40}\z}.freeze
PINNED_CHECKOUT = %r{\Aactions/checkout@[0-9a-f]{40}\z}.freeze

class WorkflowViolation < StandardError; end

def violation(path, message)
  raise WorkflowViolation, "#{path.basename}: #{message}"
end

def validate_ast!(path, node)
  if node.respond_to?(:anchor) && node.anchor
    violation(path, "YAML anchors and aliases are not allowed")
  end
  if node.is_a?(Psych::Nodes::Alias)
    violation(path, "YAML anchors and aliases are not allowed")
  end

  case node
  when Psych::Nodes::Mapping
    seen = {}
    node.children.each_slice(2) do |key_node, value_node|
      unless key_node.is_a?(Psych::Nodes::Scalar)
        violation(path, "complex YAML mapping keys are not allowed")
      end
      key = key_node.value
      violation(path, "YAML merge keys are not allowed") if key == "<<"
      violation(path, "duplicate decoded YAML key #{key.inspect}") if seen.key?(key)
      seen[key] = true
      validate_ast!(path, key_node)
      validate_ast!(path, value_node)
    end
  when Psych::Nodes::Sequence, Psych::Nodes::Stream, Psych::Nodes::Document
    node.children.each { |child| validate_ast!(path, child) }
  when Psych::Nodes::Scalar
    if node.value == "pull_request_target"
      violation(path, "pull_request_target is not allowed")
    end
    if node.value.include?("cloudzombie/sov") || node.value.include?("/github/sov")
      violation(path, "workflow references the SOV repository")
    end
  end
end

def validate_runner!(path, job_name, job)
  runner = job["runs-on"]
  return if HOSTED_RUNNERS.include?(runner)

  if runner == "${{ matrix.os }}"
    strategy = job["strategy"]
    matrix = strategy.is_a?(Hash) ? strategy["matrix"] : nil
    operating_systems = matrix.is_a?(Hash) ? matrix["os"] : nil
    unless operating_systems.is_a?(Array) && !operating_systems.empty? &&
           operating_systems.all? { |item| HOSTED_RUNNERS.include?(item) }
      violation(path, "job #{job_name.inspect} has an unbounded OS matrix")
    end
    return
  end

  violation(path, "job #{job_name.inspect} must use an approved hosted runner")
end

def validate_workflow!(path)
  source = path.read
  stream = Psych.parse_stream(source, filename: path.to_s)
  unless stream.children.length == 1 && stream.children.first.root
    violation(path, "workflow must contain exactly one YAML document")
  end
  validate_ast!(path, stream)

  workflow = Psych.safe_load(
    source,
    permitted_classes: [],
    permitted_symbols: [],
    aliases: false,
    filename: path.to_s
  )
  violation(path, "workflow root must be a mapping") unless workflow.is_a?(Hash)
  unless workflow["permissions"] == { "contents" => "read" }
    violation(path, "top-level permissions must be exactly contents: read")
  end

  jobs = workflow["jobs"]
  violation(path, "jobs must be a non-empty mapping") unless jobs.is_a?(Hash) && !jobs.empty?

  checkout_count = 0
  publish_seen = false
  jobs.each do |job_name, job|
    unless job_name.is_a?(String) && job.is_a?(Hash)
      violation(path, "every job must be a named mapping")
    end
    if job.key?("uses")
      violation(path, "reusable workflow jobs are not allowed")
    end
    validate_runner!(path, job_name, job)

    permissions = job["permissions"]
    if path.basename.to_s == "release.yml" && job_name == "publish"
      publish_seen = true
      unless permissions == RELEASE_PERMISSIONS
        violation(path, "jobs.publish must have exactly the release write permissions")
      end
    elsif !permissions.nil?
      violation(path, "only release.yml jobs.publish may override permissions")
    end

    steps = job["steps"]
    violation(path, "job #{job_name.inspect} steps must be a non-empty list") unless steps.is_a?(Array) && !steps.empty?
    steps.each do |step|
      violation(path, "every step must be a mapping") unless step.is_a?(Hash)
      action = step["uses"]
      next if action.nil?
      unless action.is_a?(String) && (action.start_with?("./") || PINNED_ACTION.match?(action))
        violation(path, "action is not pinned to a full commit: #{action.inspect}")
      end
      next unless PINNED_CHECKOUT.match?(action)

      checkout_count += 1
      violation(path, "publish job must never checkout source") if job_name == "publish"
      inputs = step["with"]
      unless inputs.is_a?(Hash) && inputs["persist-credentials"] == false
        violation(path, "checkout must set literal persist-credentials: false")
      end
    end
  end

  if path.basename.to_s == "release.yml"
    violation(path, "release workflow lacks jobs.publish") unless publish_seen
  elsif publish_seen
    violation(path, "non-release workflow contains a publish job override")
  end
  violation(path, "workflow has no pinned checkout step") if checkout_count.zero?
end

def run_regression_checks!
  checkout = <<~YAML
    safe:
      runs-on: ubuntu-24.04
      steps:
        - uses: actions/checkout@0000000000000000000000000000000000000000
          with:
            persist-credentials: false
  YAML
  attacks = {
    "flow permission map" => <<~YAML,
      evil: {runs-on: ubuntu-24.04, permissions: {contents: write}, steps: [{run: "true"}]}
    YAML
    "escaped permission key" => <<~YAML,
      evil:
        runs-on: ubuntu-24.04
        "permissi\\u006fns": {contents: write}
        steps: [{run: "true"}]
    YAML
    "bare tagged permission key" => <<~YAML,
      evil:
        runs-on: ubuntu-24.04
        ! permissions: {contents: write}
        steps: [{run: "true"}]
    YAML
    "quoted checkout key" => <<~YAML,
      evil:
        runs-on: ubuntu-24.04
        steps:
          - "uses": actions/checkout@0000000000000000000000000000000000000000
    YAML
    "flow unpinned action" => <<~YAML,
      evil:
        runs-on: ubuntu-24.04
        steps: [{uses: "actions/checkout@main"}]
    YAML
    "aliased permissions" => <<~YAML,
      evil:
        runs-on: ubuntu-24.04
        permissions: &danger {contents: write}
        steps: [{run: "true"}]
    YAML
    "duplicate decoded permission key" => <<~YAML
      evil:
        runs-on: ubuntu-24.04
        permissions: {contents: read}
        "permissi\\u006fns": {contents: write}
        steps: [{run: "true"}]
    YAML
  }

  attacks.each do |label, attack|
    source = +<<~YAML
      name: checker-regression
      on: workflow_dispatch
      permissions: {contents: read}
      jobs:
    YAML
    source << checkout.lines.map { |line| "  #{line}" }.join
    source << attack.lines.map { |line| "  #{line}" }.join
    rejected = false
    Tempfile.create(["xus-workflow-check-", ".yml"]) do |file|
      file.write(source)
      file.flush
      begin
        validate_workflow!(Pathname.new(file.path))
      rescue Psych::Exception, WorkflowViolation
        rejected = true
      end
    end
    raise WorkflowViolation, "checker regression accepted #{label}" unless rejected
  end
end

begin
  run_regression_checks!
  workflows = WORKFLOW_DIR.glob("*.{yml,yaml}").sort
  raise WorkflowViolation, "no workflow files found" if workflows.empty?
  workflows.each { |path| validate_workflow!(path) }
  puts "workflow boundary: clean"
rescue Psych::Exception, WorkflowViolation => error
  warn "workflow boundary violation: #{error.message}"
  exit 1
end
