# SSH Troubleshooting:
# - If you get the error "Build Failed: ImageBuild: could not parse ssh: [default]:...":
#   Make sure the ssh-agent is running BEFORE you run `tilt up` by running the command:
#     eval $(ssh-agent) && ssh-add
#   (you should run this in the same terminal session as you run `tilt up` in)

include('../relay/Tiltfile')

env={
  'LISTEN_ADDRESS': 'localhost:3002',
  'FIBERPLANE_ENDPOINT': 'ws://localhost:3001' if os.getenv('LOCAL_PROXY') or os.getenv('LOCAL_RELAY') else 'ws://relay',
  'AUTH_TOKEN':'MVPpfxAYRxcQ4rFZUB7RRzirzwhR7htlkU3zcDm-pZk',
}

if os.getenv('LOCAL_PROXY'):
  local_resource('proxy', 
    serve_env=env,
    serve_cmd='cargo run --bin proxy',
    dir='proxy',
    deps=['Cargo.toml', 'Cargo.lock', 'src', 'migrations'], 
    resource_deps=['relay'], 
    readiness_probe=probe(http_get=http_get_action(3002, path='/healthz')))
else:
  # Run docker with ssh option to access private git repositories
  docker_build('proxy:latest', '.', dockerfile='./Dockerfile.dev', ssh='default')
  k8s_resource(workload='proxy', resource_deps=['relay'], port_forwards=3002, labels=['customer'])

  k8s_yaml(local('./scripts/template.sh deployment/deployment.template.yaml', env=env))
