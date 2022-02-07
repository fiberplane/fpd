# SSH Troubleshooting:
# - If you get the error "Build Failed: ImageBuild: could not parse ssh: [default]:...":
#   Make sure the ssh-agent is running BEFORE you run `tilt up` by running the command:
#     eval $(ssh-agent) && ssh-add
#   (you should run this in the same terminal session as you run `tilt up` in)

include('../relay/Tiltfile')

proxy_token = 'MVPpfxAYRxcQ4rFZUB7RRzirzwhR7htlkU3zcDm-pZk';
fiberplane_endpoint = 'ws://localhost:3001' if os.getenv('LOCAL_PROXY') or os.getenv('LOCAL_RELAY') else 'ws://relay'

if os.getenv('LOCAL_PROXY'):
  local_resource('proxy', 
    serve_env={
      'LISTEN_ADDRESS': 'localhost:3002',
      'FIBERPLANE_ENDPOINT': 'ws://localhost:3001',
      'AUTH_TOKEN': proxy_token
    },
    serve_cmd='cargo run --bin proxy',
    dir='proxy',
    deps=['Cargo.toml', 'Cargo.lock', 'src', 'migrations'], 
    resource_deps=['relay'], 
    readiness_probe=probe(http_get=http_get_action(3002, path='/healthz')))
else:
  # Run docker with ssh option to access private git repositories
  docker_build('proxy:latest', '.', dockerfile='./Dockerfile.dev', ssh='default')
  k8s_resource(workload='proxy', resource_deps=['relay'], port_forwards=3002, labels=['customer'])

  k8s_yaml(blob('''
  apiVersion: apps/v1
  kind: Deployment
  metadata:
    name: proxy
    labels:
      app: proxy
  spec:
    selector:
      matchLabels:
        app: proxy
    template:
      metadata:
        labels:
          app: proxy
      spec:
        containers:
        - name: proxy
          image: proxy:latest
          imagePullPolicy: Always
          env:
            - name: RUST_LOG
              value: proxy=trace
            - name: AUTH_TOKEN
              value: {}
            - name: LISTEN_ADDRESS
              value: 0.0.0.0:3002
            - name: FIBERPLANE_ENDPOINT
              value: {}
          ports:
          - containerPort: 3002
  '''.format(proxy_token, fiberplane_endpoint)))
